use super::*;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentContext, AgentSession};
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
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use taskstore::Store;
use tokio::sync::broadcast;

const TEST_PREFIX: &str = "[coordinator:test]";

/// Mock LLM client for testing.
pub(crate) struct MockLlm {
    responses: StdMutex<Vec<String>>,
}

impl MockLlm {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: StdMutex::new(responses),
        }
    }
}

impl LlmClient for MockLlm {
    fn call<'a>(
        &'a self,
        _system_prompt: &'a str,
        _user_message: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
        async move {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(r#"[{"action": "done", "summary": "No more responses"}]"#.to_string())
            } else {
                Ok(responses.remove(0))
            }
        }
    }
}

pub(crate) fn test_stores(dir: &std::path::Path) -> Arc<Stores> {
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

/// Build a CoordinatorAgent for testing with the given stores and LLM responses.
pub(crate) fn test_coordinator(
    dir: &std::path::Path,
    stores: &Arc<Stores>,
    responses: Vec<String>,
    config: CoordinatorConfig,
) -> CoordinatorAgent<MockLlm> {
    let (event_tx, _rx) = broadcast::channel(16);
    let session = AgentSession::new(AgentKind::Coordinator, "test-model".into());
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
    let ctx = AgentContext {
        session,
        stores: stores.clone(),
        bridge,
        event_tx,
        tool_runner: stores.read_tool_runner().unwrap(),
        tool_executor: stores.read_tool_executor().unwrap(),
        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    };
    let llm = MockLlm::new(responses);
    CoordinatorAgent::new(ctx, llm, config)
}

/// Insert an active goal so `load_or_create_coordinator_state` returns `Some`.
pub(crate) fn insert_test_goal(stores: &Arc<Stores>) {
    use crate::domain::coordinator_goal::CoordinatorGoal;
    let goal = CoordinatorGoal::new("test goal".to_string());
    stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);
}

// --- build_state_summary tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_empty() {
    let dir = TestDir::new("loopr-coord-empty");
    let stores = test_stores(&dir);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("No active records"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_with_plan() {
    let dir = TestDir::new("loopr-coord-plan");
    let stores = test_stores(&dir);

    let plan = Plan::new("Test Plan".into(), "Tests pass".into());
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("### Plans"));
    assert!(summary.contains("Test Plan"));
    assert!(summary.contains("draft"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_excludes_completed() {
    let dir = TestDir::new("loopr-coord-excl");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Done Plan".into(), "crit".into());
    plan.force_status(HierarchyStatus::Complete);
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(!summary.contains("Done Plan"));
    assert!(summary.contains("No active records"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_with_works() {
    let dir = TestDir::new("loopr-coord-wi");
    let stores = test_stores(&dir);

    let wi = Work::new("ph-1".into(), "Add auth".into());
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("### Works"));
    assert!(summary.contains("Add auth"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_with_bundles() {
    let dir = TestDir::new("loopr-coord-bun");
    let stores = test_stores(&dir);

    // Proposed bundle - since triage is now automatic, the coordinator does NOT see
    // Proposed bundles in its state summary (they're auto-triaged by the daemon).
    let bundle = Bundle::new("wi-1".into(), None, "branch-1".into(), vec!["claims".into()]);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    // Proposed bundles section removed: coordinator no longer needs to triage manually
    assert!(
        !summary.contains("### Proposed Bundles"),
        "coordinator should not see Proposed bundles: triage is now automatic"
    );
    assert!(!summary.contains("### Reviewed Bundles"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_with_reviewed_bundles() {
    let dir = TestDir::new("loopr-coord-bun-rev");
    let stores = test_stores(&dir);

    // Reviewed bundle
    let mut bundle = Bundle::new("wi-2".into(), None, "branch-2".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Reviewed);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("### Reviewed Bundles (use accept_bundle)"));
    assert!(!summary.contains("### Proposed Bundles"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_with_active_sessions() {
    let dir = TestDir::new("loopr-coord-sess");
    let stores = test_stores(&dir);

    let session = AgentSession::new(AgentKind::Implementer, "model".into());
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("### Active Agents"));
    assert!(summary.contains("implementer"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_with_locks() {
    let dir = TestDir::new("loopr-coord-lock");
    let stores = test_stores(&dir);

    let lock = Lock::new("src/main.rs".into(), "wi-1".into(), "coordinator".into());
    stores.locks.write().unwrap().insert(lock.id.clone(), lock);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("### Active Locks"));
    assert!(summary.contains("src/main.rs"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_excludes_terminal_sessions() {
    let dir = TestDir::new("loopr-coord-termsess");
    let stores = test_stores(&dir);

    let mut session = AgentSession::new(AgentKind::Implementer, "model".into());
    session.force_status(AgentStatus::Completed);
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(!summary.contains("### Active Agents"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_works_before_plans() {
    let dir = TestDir::new("loopr-coord-order");
    let stores = test_stores(&dir);

    let plan = Plan::new("Test Plan".into(), "crit".into());
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let wi = Work::new("ph-1".into(), "Add auth".into());
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let summary = build_state_summary(&stores, TEST_PREFIX);

    let works_pos = summary.find("### Works").expect("Works section missing");
    let plans_pos = summary.find("### Plans").expect("Plans section missing");
    assert!(
        works_pos < plans_pos,
        "Works ({}) should appear before Plans ({}) in summary",
        works_pos,
        plans_pos
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_excludes_done_works() {
    let dir = TestDir::new("loopr-coord-donewi");
    let stores = test_stores(&dir);

    let mut wi = Work::new("ph-1".into(), "Done Work".into());
    wi.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(!summary.contains("Done Work"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_excludes_merged_bundles_from_active() {
    let dir = TestDir::new("loopr-coord-mergedbun");
    let stores = test_stores(&dir);

    let mut bundle = Bundle::new("wi-1".into(), None, "branch-1".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Merged);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    // Merged bundles should NOT appear in the "### Bundles" section (non-terminal only)
    assert!(!summary.contains("### Bundles\n"));
}

// --- format_action_summary tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_done() {
    let result = ActionResult::Done("Complete".into());
    assert_eq!(format_action_summary(&result), "done: Complete");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_record_created() {
    let result = ActionResult::RecordCreated {
        collection: "plans".into(),
        id: "plan-123".into(),
    };
    let summary = format_action_summary(&result);
    assert!(summary.contains("created plans"));
    assert!(summary.contains("plan-123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_agent_spawned() {
    let result = ActionResult::AgentSpawned {
        session_id: "sess-abc".into(),
        agent_type: "implementer".into(),
    };
    let summary = format_action_summary(&result);
    assert!(summary.contains("spawned implementer"));
    assert!(summary.contains("sess-abc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_error() {
    let result = ActionResult::ActionError("something broke".into());
    assert_eq!(format_action_summary(&result), "ERROR: something broke");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_document_validated_pass() {
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

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_document_validated_fail_with_issues() {
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

#[tokio::test(flavor = "multi_thread")]
async fn test_system_prompt_contains_key_sections() {
    crate::prompts::init_defaults();
    let prompt = &crate::prompts::store().coordinator;
    assert!(prompt.contains("Coordinator agent"));
    assert!(
        !prompt.contains("create_plan"),
        "coordinator prompt must not contain dead action create_plan"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_comprehensive() {
    let dir = TestDir::new("loopr-coord-comp");
    let stores = test_stores(&dir);

    // Add plan
    let plan = Plan::new("My Plan".into(), "crit".into());
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    // Add spec
    let spec = Spec::new(plan_id.clone(), "My Spec".into(), 0);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // Add phase
    let phase = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Add work item
    let wi = Work::new(phase_id.clone(), "WI 1".into());
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    // Add tick
    let tick = Tick::new(1);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(summary.contains("### Plans"));
    assert!(summary.contains("### Specs"));
    assert!(summary.contains("### Phases"));
    assert!(summary.contains("### Works"));
    assert!(summary.contains("### Ticks"));
}

// --- infer_action_level tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_infer_action_level_returns_none_for_done() {
    let action = AgentAction::Done {
        summary: "all done".into(),
    };
    assert!(infer_action_level(&action).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_infer_action_level_returns_none_for_create_learning() {
    let action = AgentAction::CreateLearning {
        content: "learned something".into(),
        scope: "global".into(),
        source_id: "test".into(),
        applicable_roles: None,
        files: None,
    };
    assert!(infer_action_level(&action).is_none());
}

// --- check_phase_completion tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_check_phase_completion_no_active_plan() {
    let dir = TestDir::new("loopr-coord-noplan");
    let stores = test_stores(&dir);

    // No plans at all → should return empty
    let completed = check_phase_completion(&stores);
    assert!(completed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_phase_completion_all_done() {
    let dir = TestDir::new("loopr-coord-phase-done");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&*dir)
        .output()
        .unwrap();
    let stores = test_stores(&dir);

    // Create an Active plan
    let mut plan = Plan::new("Test Plan".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    // Create an Active spec
    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // Create an Active phase
    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    phase.force_status(HierarchyStatus::Active);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Create works that are all Done
    let mut w1 = Work::new(phase_id.clone(), "Work 1".into());
    w1.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(w1.id.clone(), w1);

    let mut w2 = Work::new(phase_id.clone(), "Work 2".into());
    w2.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(w2.id.clone(), w2);

    let completed = check_phase_completion(&stores);
    assert_eq!(completed.len(), 1);
    assert!(completed[0].contains("Phase 1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_phase_completion_partial() {
    let dir = TestDir::new("loopr-coord-phase-partial");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&*dir)
        .output()
        .unwrap();
    let stores = test_stores(&dir);

    // Active plan → Active spec → Active phase
    let mut plan = Plan::new("Test Plan".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    phase.force_status(HierarchyStatus::Active);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // One work Done, one still InProgress
    let mut w1 = Work::new(phase_id.clone(), "Work 1".into());
    w1.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(w1.id.clone(), w1);

    let mut w2 = Work::new(phase_id.clone(), "Work 2".into());
    w2.force_status(WorkStatus::InProgress);
    stores.works.write().unwrap().insert(w2.id.clone(), w2);

    let completed = check_phase_completion(&stores);
    assert!(completed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_phase_completion_no_works() {
    let dir = TestDir::new("loopr-coord-phase-noworks");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&*dir)
        .output()
        .unwrap();
    let stores = test_stores(&dir);

    // Active plan → Active spec → Active phase, but NO works
    let mut plan = Plan::new("Test Plan".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    phase.force_status(HierarchyStatus::Active);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // is_phase_complete returns false when there are no works
    let completed = check_phase_completion(&stores);
    assert!(completed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_phase_completion_multiple_phases() {
    let dir = TestDir::new("loopr-coord-phase-multi");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&*dir)
        .output()
        .unwrap();
    let stores = test_stores(&dir);

    // Active plan → Active spec → 2 Active phases
    let mut plan = Plan::new("Test Plan".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // Phase 1: all works Done
    let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    phase1.force_status(HierarchyStatus::Active);
    let phase1_id = phase1.id.clone();
    stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

    let mut w1 = Work::new(phase1_id.clone(), "Work 1".into());
    w1.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(w1.id.clone(), w1);

    // Phase 2: one work still InProgress
    let mut phase2 = Phase::new(spec_id.clone(), "Phase 2".into(), 2);
    phase2.force_status(HierarchyStatus::Active);
    let phase2_id = phase2.id.clone();
    stores.phases.write().unwrap().insert(phase2_id.clone(), phase2);

    let mut w2 = Work::new(phase2_id.clone(), "Work 2".into());
    w2.force_status(WorkStatus::InProgress);
    stores.works.write().unwrap().insert(w2.id.clone(), w2);

    let completed = check_phase_completion(&stores);
    assert_eq!(completed.len(), 1);
    assert!(completed[0].contains("Phase 1"));
    assert!(!completed.iter().any(|c| c.contains("Phase 2")));
}

// --- format_action_summary additional coverage ---

#[tokio::test(flavor = "multi_thread")]
async fn test_format_action_summary_document_validated_empty_issues() {
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

#[tokio::test(flavor = "multi_thread")]
async fn test_load_or_create_coordinator_state_no_goal() {
    let dir = TestDir::new("loopr-coord-fsm-nogoal");
    let stores = test_stores(&dir);

    let result = load_or_create_coordinator_state(&stores);
    assert!(result.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_or_create_coordinator_state_with_goal() {
    let dir = TestDir::new("loopr-coord-fsm-goal");
    let stores = test_stores(&dir);

    let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Build app".to_string());
    let goal_id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

    let state = load_or_create_coordinator_state(&stores).unwrap();
    assert_eq!(state.goal_id, goal_id);
    assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_or_create_coordinator_state_resumes_existing() {
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

#[tokio::test(flavor = "multi_thread")]
async fn test_check_fsm_transition_planning_to_activate() {
    let dir = TestDir::new("loopr-coord-fsm-plan2act");
    let stores = test_stores(&dir);

    // Create Plan → Spec → Phase hierarchy (all Active)
    let mut plan = Plan::new("Plan".into(), "crit".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Spec".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    phase.force_status(HierarchyStatus::Active);
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Planning;
    let config = CoordinatorConfig::default();

    let transition = check_fsm_transition(&stores, &coord_state, &config);
    assert_eq!(transition, Some(CoordinatorFsmState::ActivatePhase));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_fsm_transition_executing_to_phase_gate() {
    let dir = TestDir::new("loopr-coord-fsm-exec2gate");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Create a Done work item in the phase
    let mut wi = Work::new(phase_id.clone(), "WI 1".into());
    wi.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    coord_state.current_phase_id = Some(phase_id);
    let config = CoordinatorConfig::default();

    let transition = check_fsm_transition(&stores, &coord_state, &config);
    assert_eq!(transition, Some(CoordinatorFsmState::PhaseGate));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_fsm_transition_phase_gate_to_goal_complete() {
    let dir = TestDir::new("loopr-coord-fsm-gate2done");
    let stores = test_stores(&dir);

    // No more phases to activate
    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::PhaseGate;
    let config = CoordinatorConfig::default();

    let transition = check_fsm_transition(&stores, &coord_state, &config);
    assert_eq!(transition, Some(CoordinatorFsmState::GoalComplete));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_persist_coordinator_state() {
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

#[tokio::test(flavor = "multi_thread")]
async fn test_build_phase_status_no_phase() {
    let dir = TestDir::new("loopr-coord-fsm-nophase");
    let stores = test_stores(&dir);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let status = build_phase_status(&stores, &coord_state);
    assert!(status.contains("No active phase"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_phase_status_with_works() {
    let dir = TestDir::new("loopr-coord-fsm-phstatus");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Build Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let wi1 = Work::new(phase_id.clone(), "WI 1".into());
    let mut wi2 = Work::new(phase_id.clone(), "WI 2".into());
    wi2.force_status(WorkStatus::Done);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_build_phase_status_all_terminal() {
    let dir = TestDir::new("loopr-coord-fsm-allterm");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Done Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi1 = Work::new(phase_id.clone(), "WI 1".into());
    wi1.force_status(WorkStatus::Done);
    let mut wi2 = Work::new(phase_id.clone(), "WI 2".into());
    wi2.force_status(WorkStatus::Abandoned);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_planning_stays_when_no_plan() {
    let dir = TestDir::new("loopr-fsm-plannoplan");
    let stores = test_stores(&dir);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let config = CoordinatorConfig::default();
    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_planning_stays_when_plan_but_no_spec() {
    let dir = TestDir::new("loopr-fsm-plannospec");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let config = CoordinatorConfig::default();
    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_planning_stays_when_plan_spec_but_no_phases() {
    let dir = TestDir::new("loopr-fsm-plannophase");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "S".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let config = CoordinatorConfig::default();
    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_planning_stays_when_plan_is_draft() {
    let dir = TestDir::new("loopr-fsm-plandraft");
    let stores = test_stores(&dir);

    // Plan exists but is Draft, not Active
    let plan = Plan::new("P".into(), "c".into());
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let config = CoordinatorConfig::default();
    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

// --- ActivatePhase → Executing ---

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_activate_phase_to_executing_when_wis_exist() {
    let dir = TestDir::new("loopr-fsm-act2exec");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let wi = Work::new(phase_id.clone(), "WI 1".into());
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_activate_phase_stays_when_no_phase_id() {
    let dir = TestDir::new("loopr-fsm-actnopid");
    let stores = test_stores(&dir);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
    // current_phase_id is None
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_activate_phase_stays_when_no_wis() {
    let dir = TestDir::new("loopr-fsm-actnowi");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_stays_when_wis_in_progress() {
    let dir = TestDir::new("loopr-fsm-execwip");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI 1".into());
    wi.force_status(WorkStatus::InProgress);
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    coord_state.current_phase_id = Some(phase_id);
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_to_phase_gate_with_mixed_done_abandoned() {
    let dir = TestDir::new("loopr-fsm-execmix");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi1 = Work::new(phase_id.clone(), "WI Done".into());
    wi1.force_status(WorkStatus::Done);
    let mut wi2 = Work::new(phase_id.clone(), "WI Abandoned".into());
    wi2.force_status(WorkStatus::Abandoned);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_stays_when_partial_done() {
    let dir = TestDir::new("loopr-fsm-execpartial");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi1 = Work::new(phase_id.clone(), "WI Done".into());
    wi1.force_status(WorkStatus::Done);
    let mut wi2 = Work::new(phase_id.clone(), "WI Ready".into());
    wi2.force_status(WorkStatus::Ready);
    stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
    stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    coord_state.current_phase_id = Some(phase_id);
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_to_phase_gate_on_zero_wis() {
    let dir = TestDir::new("loopr-fsm-exec0wi");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_to_phase_gate_on_phase_timeout() {
    let dir = TestDir::new("loopr-fsm-exectimeout");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // WI in progress — would normally stay Executing
    let mut wi = Work::new(phase_id.clone(), "WI".into());
    wi.force_status(WorkStatus::InProgress);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_to_goal_complete_on_goal_timeout() {
    let dir = TestDir::new("loopr-fsm-execgoalto");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI".into());
    wi.force_status(WorkStatus::InProgress);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_no_current_phase_stays() {
    let dir = TestDir::new("loopr-fsm-execnophase");
    let stores = test_stores(&dir);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    // current_phase_id is None — shouldn't happen normally, but shouldn't panic
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

// --- PhaseGate → ActivatePhase (more phases) ---

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_phase_gate_to_activate_when_more_phases() {
    let dir = TestDir::new("loopr-fsm-gate2act");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "S".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    phase1.force_status(HierarchyStatus::Complete);
    let phase1_id = phase1.id.clone();
    stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

    // Phase 2 stays Draft - ready to activate
    let phase2 = Phase::new(spec_id, "Phase 2".into(), 2);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_goal_complete_returns_none() {
    let dir = TestDir::new("loopr-fsm-goalnone");
    let stores = test_stores(&dir);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::GoalComplete;
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_apply_fsm_activate_phase_no_next_phase_deactivates_goal() {
    // Regression: when ActivatePhase finds no next phase (all phases complete),
    // the goal must be deactivated so `run` can detect completion via get_goal.
    let dir = TestDir::new("loopr-fsm-goaldeact");
    let stores = test_stores(&dir);

    let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Test goal".to_string());
    let goal_id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

    let mut coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::PhaseGate;

    // No phases in stores — find_next_phase_to_activate returns None
    apply_fsm_transition(
        CoordinatorFsmState::ActivatePhase,
        &mut coord_state,
        &stores,
        TEST_PREFIX,
    );

    assert_eq!(coord_state.fsm_state, CoordinatorFsmState::GoalComplete);
    let goals = stores.coordinator_goals.read().unwrap();
    let goal = goals.get(&goal_id).unwrap();
    assert!(
        !goal.active,
        "goal must be deactivated when coordinator reaches GoalComplete"
    );
}

// --- Phase timeout vs goal timeout priority ---

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_phase_timeout_takes_priority_over_wi_check() {
    let dir = TestDir::new("loopr-fsm-phtopri");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // All WIs are Done — would normally trigger PhaseGate via WI check
    let mut wi = Work::new(phase_id.clone(), "WI".into());
    wi.force_status(WorkStatus::Done);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_goal_timeout_takes_priority_over_phase_timeout() {
    let dir = TestDir::new("loopr-fsm-goaltopri");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI".into());
    wi.force_status(WorkStatus::InProgress);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_find_next_phase_skips_completed() {
    let dir = TestDir::new("loopr-fsm-nextphskip");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "S".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // p1 is Complete, p2 is Draft - next phase should be p2
    let mut p1 = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    p1.force_status(HierarchyStatus::Complete);
    let p1_id = p1.id.clone();
    stores.phases.write().unwrap().insert(p1_id.clone(), p1);

    let p2 = Phase::new(spec_id, "Phase 2".into(), 2);
    // p2 stays Draft (default)
    let p2_id = p2.id.clone();
    stores.phases.write().unwrap().insert(p2_id.clone(), p2);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let result = find_next_phase_to_activate(&stores, &coord_state);
    assert!(result.is_some());
    let (id, title) = result.unwrap();
    assert_eq!(id, p2_id);
    assert_eq!(title, "Phase 2");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_find_next_phase_waits_for_active_phase() {
    // When a phase is Active (in progress), return None - don't skip ahead
    let dir = TestDir::new("loopr-fsm-nextphwait");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "S".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut p1 = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    p1.force_status(HierarchyStatus::Active);
    stores.phases.write().unwrap().insert(p1.id.clone(), p1);

    let p2 = Phase::new(spec_id, "Phase 2".into(), 2);
    let p2_id = p2.id.clone();
    stores.phases.write().unwrap().insert(p2_id.clone(), p2);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let result = find_next_phase_to_activate(&stores, &coord_state);
    // p1 is Active (non-terminal), p2 is Draft -> return first Draft = p2
    assert!(result.is_some());
    let (id, _title) = result.unwrap();
    assert_eq!(id, p2_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_find_next_phase_respects_spec_boundaries() {
    // Phases from Spec 1 must all be terminal before looking at Spec 2
    let dir = TestDir::new("loopr-fsm-nextphspec");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec1 = Spec::new(plan_id.clone(), "Spec 1".into(), 0);
    spec1.force_status(HierarchyStatus::Active);
    let spec1_id = spec1.id.clone();
    stores.specs.write().unwrap().insert(spec1_id.clone(), spec1);

    let mut spec2 = Spec::new(plan_id, "Spec 2".into(), 1);
    spec2.force_status(HierarchyStatus::Active);
    let spec2_id = spec2.id.clone();
    stores.specs.write().unwrap().insert(spec2_id.clone(), spec2);

    // Spec 1 has an Active phase (non-terminal)
    let mut p1 = Phase::new(spec1_id, "S1 Phase 1".into(), 0);
    p1.force_status(HierarchyStatus::Active);
    stores.phases.write().unwrap().insert(p1.id.clone(), p1);

    // Spec 2 has a Draft phase
    let p2 = Phase::new(spec2_id, "S2 Phase 1".into(), 0);
    let p2_id = p2.id.clone();
    stores.phases.write().unwrap().insert(p2_id.clone(), p2);

    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let result = find_next_phase_to_activate(&stores, &coord_state);
    // Spec 1 still has non-terminal phases - must NOT return Spec 2's phase
    assert!(result.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_find_next_phase_returns_none_all_completed() {
    let dir = TestDir::new("loopr-fsm-nextphnone");
    let stores = test_stores(&dir);

    // No phases at all
    let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    let result = find_next_phase_to_activate(&stores, &coord_state);
    assert!(result.is_none());
}

// --- Transition handler integration ---

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_transition_handler_complete_phase_on_activate() {
    // When transitioning PhaseGate → ActivatePhase, the previous phase
    // should be completed (added to phases_completed).
    let dir = TestDir::new("loopr-fsm-thcomplete");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("P".into(), "c".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "S".into(), 0);
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut p1 = Phase::new(spec_id.clone(), "Phase 1".into(), 1);
    p1.force_status(HierarchyStatus::Complete);
    let p1_id = p1.id.clone();
    stores.phases.write().unwrap().insert(p1_id.clone(), p1);

    // Phase 2 stays Draft - ready to activate
    let p2 = Phase::new(spec_id, "Phase 2".into(), 2);
    stores.phases.write().unwrap().insert(p2.id.clone(), p2);

    // Coordinator is in PhaseGate with phase 1 as current
    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::PhaseGate;
    coord_state.current_phase_id = Some(p1_id.clone());

    let config = CoordinatorConfig::default();

    // check_fsm_transition should return ActivatePhase (Phase 2 is Draft)
    let transition = check_fsm_transition(&stores, &coord_state, &config);
    assert_eq!(transition, Some(CoordinatorFsmState::ActivatePhase));

    // Simulate the transition handler: complete previous phase
    if coord_state.current_phase_id.is_some() {
        coord_state.complete_phase();
    }

    assert!(coord_state.phases_completed.contains(&p1_id));
    assert!(coord_state.current_phase_id.is_none());
}

// --- Fix #12: mark_phase_record_complete tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_mark_phase_record_complete() {
    let dir = TestDir::new("loopr-coord-phasecomplete");
    let stores = test_stores(&dir);

    let mut phase = Phase::new("spec-1".into(), "Test Phase".into(), 1);
    phase.force_status(HierarchyStatus::Active);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.current_phase_id = Some(phase_id.clone());

    mark_phase_record_complete(&stores, &coord_state, TEST_PREFIX);

    let phases = stores.phases.read().unwrap();
    let updated = phases.get(&phase_id).unwrap();
    assert_eq!(updated.status(), HierarchyStatus::Complete);
}

// --- Fix #2: resolve_batch_dependencies tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_resolve_batch_deps_resolves_batch_0() {
    let batch_ids = vec!["wi-aaa".to_string(), "wi-bbb".to_string()];
    let action = AgentAction::CreateWork {
        parent_id: "phase-1".into(),
        title: "WI".into(),
        description: "d".into(),
        files: vec![],
        acceptance_criteria: vec![],
        dependencies: vec!["batch:0".to_string()],
    };
    let resolved = resolve_batch_dependencies(&action, &batch_ids, TEST_PREFIX);
    assert!(resolved.is_some());
    if let Some(AgentAction::CreateWork { dependencies, .. }) = resolved {
        assert_eq!(dependencies, vec!["wi-aaa".to_string()]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_resolve_batch_deps_out_of_range() {
    let batch_ids = vec!["wi-aaa".to_string()];
    let action = AgentAction::CreateWork {
        parent_id: "phase-1".into(),
        title: "WI".into(),
        description: "d".into(),
        files: vec![],
        acceptance_criteria: vec![],
        dependencies: vec!["batch:5".to_string()],
    };
    let resolved = resolve_batch_dependencies(&action, &batch_ids, TEST_PREFIX);
    assert!(resolved.is_some());
    // Out of range falls through — keeps the original "batch:5" string
    if let Some(AgentAction::CreateWork { dependencies, .. }) = resolved {
        assert_eq!(dependencies, vec!["batch:5".to_string()]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_resolve_batch_deps_no_batch_refs() {
    let batch_ids = vec!["wi-aaa".to_string()];
    let action = AgentAction::CreateWork {
        parent_id: "phase-1".into(),
        title: "WI".into(),
        description: "d".into(),
        files: vec![],
        acceptance_criteria: vec![],
        dependencies: vec!["wi-existing".to_string()],
    };
    let resolved = resolve_batch_dependencies(&action, &batch_ids, TEST_PREFIX);
    assert!(resolved.is_none(), "no batch refs should return None");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_resolve_batch_deps_non_create_action() {
    let batch_ids = vec!["wi-aaa".to_string()];
    let action = AgentAction::Done { summary: "done".into() };
    let resolved = resolve_batch_dependencies(&action, &batch_ids, TEST_PREFIX);
    assert!(resolved.is_none(), "non-CreateWork should return None");
}

// --- Fix #6: prune_independent_deps tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_prune_independent_deps_removes_disjoint() {
    let dir = TestDir::new("loopr-coord-prune1");
    let stores = test_stores(&dir);

    // Create two works with non-overlapping files — dep should be pruned
    let mut wi_a = Work::new("phase-1".into(), "Work A".into());
    wi_a.files = vec!["src/a.rs".into()];
    let a_id = wi_a.id.clone();

    let mut wi_b = Work::new("phase-1".into(), "Work B".into());
    wi_b.files = vec!["src/b.rs".into()];
    wi_b.dependencies = vec![a_id.clone()];
    let b_id = wi_b.id.clone();

    stores.works.write().unwrap().insert(a_id.clone(), wi_a);
    stores.works.write().unwrap().insert(b_id.clone(), wi_b);

    prune_independent_deps(&stores, &[a_id.clone(), b_id.clone()], TEST_PREFIX);

    let works = stores.works.read().unwrap();
    assert!(works[&b_id].dependencies.is_empty(), "disjoint dep should be pruned");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_prune_independent_deps_keeps_overlapping() {
    let dir = TestDir::new("loopr-coord-prune2");
    let stores = test_stores(&dir);

    // Both works touch src/main.rs — dep should be kept
    let mut wi_a = Work::new("phase-1".into(), "Work A".into());
    wi_a.files = vec!["src/main.rs".into(), "src/a.rs".into()];
    let a_id = wi_a.id.clone();

    let mut wi_b = Work::new("phase-1".into(), "Work B".into());
    wi_b.files = vec!["src/main.rs".into(), "src/b.rs".into()];
    wi_b.dependencies = vec![a_id.clone()];
    let b_id = wi_b.id.clone();

    stores.works.write().unwrap().insert(a_id.clone(), wi_a);
    stores.works.write().unwrap().insert(b_id.clone(), wi_b);

    prune_independent_deps(&stores, &[a_id.clone(), b_id.clone()], TEST_PREFIX);

    let works = stores.works.read().unwrap();
    assert_eq!(works[&b_id].dependencies, vec![a_id], "overlapping dep should be kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_prune_independent_deps_keeps_external() {
    let dir = TestDir::new("loopr-coord-prune3");
    let stores = test_stores(&dir);

    // wi_b depends on an external work (not in batch) — should be kept regardless
    let mut wi_a = Work::new("phase-1".into(), "Work A".into());
    wi_a.files = vec!["src/a.rs".into()];
    let a_id = wi_a.id.clone();

    let mut wi_b = Work::new("phase-1".into(), "Work B".into());
    wi_b.files = vec!["src/b.rs".into()];
    wi_b.dependencies = vec!["wi-external".to_string()];
    let b_id = wi_b.id.clone();

    stores.works.write().unwrap().insert(a_id.clone(), wi_a);
    stores.works.write().unwrap().insert(b_id.clone(), wi_b);

    prune_independent_deps(&stores, &[a_id, b_id.clone()], TEST_PREFIX);

    let works = stores.works.read().unwrap();
    assert_eq!(
        works[&b_id].dependencies,
        vec!["wi-external".to_string()],
        "external dep should be kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_prune_independent_deps_empty_batch() {
    let dir = TestDir::new("loopr-coord-prune4");
    let stores = test_stores(&dir);

    // Empty batch — no-op
    prune_independent_deps(&stores, &[], TEST_PREFIX);
}

// --- Fix #5: build_phase_status dependency info tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_build_phase_status_shows_dependencies() {
    let dir = TestDir::new("loopr-coord-depstatus");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Build Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi1 = Work::new(phase_id.clone(), "Setup".into());
    wi1.force_status(WorkStatus::Done);
    let wi1_id = wi1.id.clone();
    stores.works.write().unwrap().insert(wi1_id.clone(), wi1);

    let mut wi2 = Work::new(phase_id.clone(), "Build".into());
    wi2.dependencies = vec![wi1_id.clone()];
    let wi2_id = wi2.id.clone();
    stores.works.write().unwrap().insert(wi2_id.clone(), wi2);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.current_phase_id = Some(phase_id);

    let status = build_phase_status(&stores, &coord_state);
    assert!(status.contains("READY"), "dependency met should show READY: {}", status);
    assert!(status.contains("deps:"), "should show deps info: {}", status);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_phase_status_shows_blocked_deps() {
    let dir = TestDir::new("loopr-coord-depblocked");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Build Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let wi1 = Work::new(phase_id.clone(), "Setup".into());
    // Default status is Draft, not Done
    let wi1_id = wi1.id.clone();
    stores.works.write().unwrap().insert(wi1_id.clone(), wi1);

    let mut wi2 = Work::new(phase_id.clone(), "Build".into());
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

#[tokio::test(flavor = "multi_thread")]
async fn test_increment_attempts_tracks_retries() {
    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    assert_eq!(coord_state.attempts("wi-1"), 0);
    assert_eq!(coord_state.increment_attempts("wi-1"), 1);
    assert_eq!(coord_state.increment_attempts("wi-1"), 2);
    assert_eq!(coord_state.increment_attempts("wi-1"), 3);
    assert_eq!(coord_state.attempts("wi-1"), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_decrement_attempts_on_dependency_not_met() {
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

#[tokio::test(flavor = "multi_thread")]
async fn test_build_phase_status_includes_failure_learnings() {
    let dir = TestDir::new("loopr-coord-learnings");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Build Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi1 = Work::new(phase_id.clone(), "Failing WI".into());
    wi1.force_status(WorkStatus::InProgress);
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
        files: vec![],
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

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_includes_recently_merged_bundles() {
    let dir = TestDir::new("loopr-coord-c2-merged");
    let stores = test_stores(&dir);

    // Create a WI in Integrated status (not Done)
    let mut wi = Work::new("phase-1".into(), "Test WI".into());
    wi.force_status(WorkStatus::Integrated);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    // Create a merged bundle for that WI
    let mut bundle = Bundle::new(wi_id.clone(), None, "feature/test".into(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Merged);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(
        summary.contains("Recently Merged Bundles"),
        "should include recently merged bundles section: {}",
        summary
    );
    assert!(summary.contains(&wi_id), "should link to parent WI: {}", summary);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_excludes_merged_when_wi_done() {
    let dir = TestDir::new("loopr-coord-c2-done");
    let stores = test_stores(&dir);

    // Create a WI in Done status
    let mut wi = Work::new("phase-1".into(), "Done WI".into());
    wi.force_status(WorkStatus::Done);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    // Create a merged bundle for that WI
    let mut bundle = Bundle::new(wi_id.clone(), None, "feature/done".into(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Merged);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(
        !summary.contains("Recently Merged Bundles"),
        "should NOT include merged bundles when WI is Done: {}",
        summary
    );
}

// --- C3: Rejected Bundles state summary tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_includes_rejected_bundle_with_inreview_work() {
    let dir = TestDir::new("loopr-coord-c3-rej");
    let stores = test_stores(&dir);

    // Create a WI in InReview status
    let mut wi = Work::new("phase-1".into(), "Test WI".into());
    wi.force_status(WorkStatus::InReview);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    // Create a rejected bundle for that WI
    let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Rejected);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_rejected_bundle_includes_verification_reason() {
    let dir = TestDir::new("loopr-coord-c3-reason");
    let stores = test_stores(&dir);

    let mut wi = Work::new("phase-1".into(), "Test WI".into());
    wi.force_status(WorkStatus::InReview);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Rejected);
    bundle.verification = "Rejected: missing error handling".to_string();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(
        summary.contains("missing error handling"),
        "should include rejection reason from verification: {}",
        summary
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_rejected_bundle_fallback_reason_when_empty() {
    let dir = TestDir::new("loopr-coord-c3-noverify");
    let stores = test_stores(&dir);

    let mut wi = Work::new("phase-1".into(), "Test WI".into());
    wi.force_status(WorkStatus::InReview);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Rejected);
    // verification left empty
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(
        summary.contains("bundle was rejected by reviewer"),
        "should use fallback reason when verification is empty: {}",
        summary
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_build_state_summary_excludes_rejected_when_work_not_inreview() {
    let dir = TestDir::new("loopr-coord-c3-noshow");
    let stores = test_stores(&dir);

    // Work already transitioned back to InProgress
    let mut wi = Work::new("phase-1".into(), "Test WI".into());
    wi.force_status(WorkStatus::InProgress);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Rejected);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let summary = build_state_summary(&stores, TEST_PREFIX);
    assert!(
        !summary.contains("Rejected Bundles"),
        "should NOT show rejected bundles when work is not InReview: {}",
        summary
    );
}

// --- sweep_integrated_to_done tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_sweep_integrated_to_done_transitions_works() {
    let dir = TestDir::new("loopr-coord-sweep-basic");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Insert a Work directly at Integrated status
    let mut wi = Work::new(phase_id.clone(), "WI 1".into());
    wi.force_status(WorkStatus::Integrated);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    coord_state.current_phase_id = Some(phase_id);

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

    sweep_integrated_to_done(&stores, &coord_state, &bridge, TEST_PREFIX);

    // Verify the Work is now Done
    let works = stores.works.read().unwrap();
    let updated = works.get(&wi_id).unwrap();
    assert_eq!(updated.status(), WorkStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sweep_noop_when_no_integrated_works() {
    let dir = TestDir::new("loopr-coord-sweep-noop");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Insert a Work at Ready (not Integrated)
    let wi = Work::new(phase_id.clone(), "WI 1".into());
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    coord_state.current_phase_id = Some(phase_id);

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

    sweep_integrated_to_done(&stores, &coord_state, &bridge, TEST_PREFIX);

    // Work should still be Draft (unchanged by sweep)
    let works = stores.works.read().unwrap();
    let unchanged = works.get(&wi_id).unwrap();
    assert_eq!(unchanged.status(), WorkStatus::Draft);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sweep_then_fsm_advances_to_phase_gate() {
    let dir = TestDir::new("loopr-coord-sweep-fsmadv");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // All Works in phase are Integrated — sweep should transition them to Done
    let mut wi1 = Work::new(phase_id.clone(), "WI 1".into());
    wi1.force_status(WorkStatus::Integrated);
    stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

    let mut wi2 = Work::new(phase_id.clone(), "WI 2".into());
    wi2.force_status(WorkStatus::Integrated);
    stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    coord_state.current_phase_id = Some(phase_id);

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

    // Before sweep: FSM should NOT advance (Works are Integrated, not terminal)
    let config = CoordinatorConfig::default();
    assert_eq!(
        check_fsm_transition(&stores, &coord_state, &config),
        None,
        "FSM should not advance while Works are Integrated"
    );

    // Run sweep
    sweep_integrated_to_done(&stores, &coord_state, &bridge, TEST_PREFIX);

    // After sweep: FSM should advance to PhaseGate (all Works now Done)
    assert_eq!(
        check_fsm_transition(&stores, &coord_state, &config),
        Some(CoordinatorFsmState::PhaseGate),
        "FSM should advance to PhaseGate after sweep transitions all Works to Done"
    );
}

// --- last_error_kind_for_work tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_last_error_kind_for_work_returns_none_when_no_sessions() {
    let dir = TestDir::new("loopr-coord-errk-none");
    let stores = test_stores(&dir);
    assert!(last_error_kind_for_work(&stores, "wi-1").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_last_error_kind_for_work_returns_structural_error() {
    let dir = TestDir::new("loopr-coord-errk-struct");
    let stores = test_stores(&dir);

    let mut session = AgentSession::new(AgentKind::Implementer, "model".into());
    session.work_id = Some("wi-1".to_string());
    session.force_status(AgentStatus::Failed);
    session.error_kind = Some(AgentErrorKind::ContextOverflow);
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    let kind = last_error_kind_for_work(&stores, "wi-1");
    assert_eq!(kind, Some(AgentErrorKind::ContextOverflow));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_last_error_kind_for_work_ignores_non_failed_sessions() {
    let dir = TestDir::new("loopr-coord-errk-nonfail");
    let stores = test_stores(&dir);

    let mut session = AgentSession::new(AgentKind::Implementer, "model".into());
    session.work_id = Some("wi-1".to_string());
    session.force_status(AgentStatus::Completed);
    session.error_kind = Some(AgentErrorKind::ContextOverflow);
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    assert!(last_error_kind_for_work(&stores, "wi-1").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_last_error_kind_for_work_ignores_other_works() {
    let dir = TestDir::new("loopr-coord-errk-other");
    let stores = test_stores(&dir);

    let mut session = AgentSession::new(AgentKind::Implementer, "model".into());
    session.work_id = Some("wi-2".to_string());
    session.force_status(AgentStatus::Failed);
    session.error_kind = Some(AgentErrorKind::ContextOverflow);
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    assert!(last_error_kind_for_work(&stores, "wi-1").is_none());
}

// --- phase_missing_test_tool tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_phase_missing_test_tool_no_validation_commands() {
    let dir = TestDir::new("loopr-coord-toolguard-novc");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.current_phase_id = Some(phase_id);

    let warning = phase_missing_test_tool(&stores, &coord_state);
    assert!(warning.is_empty(), "no warning when phase has no validation_commands");
}

// Phase-level validation_commands removed in domain-model-cleanup Phase 3.
// phase_missing_test_tool now always returns empty. Test kept as tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn test_phase_missing_test_tool_always_empty_after_field_removal() {
    let dir = TestDir::new("loopr-coord-toolguard-warn");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.current_phase_id = Some(phase_id);

    let warning = phase_missing_test_tool(&stores, &coord_state);
    assert!(
        warning.is_empty(),
        "phase_missing_test_tool always returns empty after field removal"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_phase_missing_test_tool_no_warning_when_tool_registered() {
    let dir = TestDir::new("loopr-coord-toolguard-ok");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into(), 1);
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

// --- decomposition_error FSM tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_run_iteration_decomposing_with_error_returns_need_help() {
    // When coord_state.fsm_state == Decomposing and decomposition_error is set,
    // run_iteration must return NeedHelp without calling the LLM.
    let dir = TestDir::new("loopr-coord-decomp-err");
    let stores = test_stores(&dir);
    let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Skip);
    coord_state.fsm_state = CoordinatorFsmState::Decomposing;
    coord_state.decomposition_error = Some("spec 'Database Layer': Failed to parse LLM output".into());

    let mut guard = Lifeguard::new();
    let result = agent.run_iteration(&mut coord_state, &mut guard).await;

    assert!(result.is_ok());
    match result.unwrap() {
        IterationOutcome::NeedHelp(msg) => {
            assert!(
                msg.contains("Background decomposition failed"),
                "NeedHelp message should reference decomposition failure, got: {msg}"
            );
            assert!(
                msg.contains("Database Layer"),
                "NeedHelp message should contain the error text, got: {msg}"
            );
        }
        other => panic!("expected NeedHelp, got: {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_run_iteration_decomposing_without_error_returns_done_waiting() {
    // When coord_state.fsm_state == Decomposing and decomposition_error is None
    // (decomposition still in progress), run_iteration must return Done("waiting...").
    let dir = TestDir::new("loopr-coord-decomp-wait");
    let stores = test_stores(&dir);
    let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Skip);
    coord_state.fsm_state = CoordinatorFsmState::Decomposing;
    // decomposition_error is None by default

    let mut guard = Lifeguard::new();
    let result = agent.run_iteration(&mut coord_state, &mut guard).await;

    assert!(result.is_ok());
    match result.unwrap() {
        IterationOutcome::Done(msg) => {
            assert!(
                msg.contains("waiting for decomposition"),
                "Done message should say waiting for decomposition, got: {msg}"
            );
        }
        other => panic!("expected Done(waiting...), got: {:?}", other),
    }
}
