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
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );
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
    let spec = Spec::new(plan_id.clone(), "My Spec".into());
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // Add phase
    let phase = Phase::new(spec_id.clone(), "Phase 1".into());
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
    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // Create an Active phase
    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into());
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

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into());
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

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "Phase 1".into());
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

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    // Phase 1: all works Done
    let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into());
    phase1.force_status(HierarchyStatus::Active);
    let phase1_id = phase1.id.clone();
    stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

    let mut w1 = Work::new(phase1_id.clone(), "Work 1".into());
    w1.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(w1.id.clone(), w1);

    // Phase 2: one work still InProgress
    let mut phase2 = Phase::new(spec_id.clone(), "Phase 2".into());
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
    let existing_id = existing.id.clone();
    stores
        .coordinator_states
        .write()
        .unwrap()
        .insert(existing_id.clone(), existing);

    let state = load_or_create_coordinator_state(&stores).unwrap();
    assert_eq!(state.id, existing_id);
    assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_fsm_transition_planning_to_executing() {
    let dir = TestDir::new("loopr-coord-fsm-plan2exec");
    let stores = test_stores(&dir);

    // Create Plan → Spec hierarchy (all Active)
    let mut plan = Plan::new("Plan".into(), "crit".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Planning;
    let config = CoordinatorConfig::default();

    let transition = check_fsm_transition(&stores, &coord_state, &config);
    assert_eq!(transition, Some(CoordinatorFsmState::Executing));
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

    let mut spec = Spec::new(plan_id, "S".into());
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

// --- Executing state: all branches ---

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_stays_when_wis_in_progress() {
    let dir = TestDir::new("loopr-fsm-execwip");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI 1".into());
    wi.force_status(WorkStatus::InProgress);
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_stays_when_partial_done() {
    let dir = TestDir::new("loopr-fsm-execpartial");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
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
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_executing_to_goal_complete_on_goal_timeout() {
    let dir = TestDir::new("loopr-fsm-execgoalto");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI".into());
    wi.force_status(WorkStatus::InProgress);
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
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
    let config = CoordinatorConfig::default();

    assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
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
async fn test_apply_fsm_goal_complete_deactivates_goal() {
    // When transitioning to GoalComplete, the goal must be deactivated.
    let dir = TestDir::new("loopr-fsm-goaldeact");
    let stores = test_stores(&dir);

    let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Test goal".to_string());
    let goal_id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

    let mut coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    apply_fsm_transition(
        CoordinatorFsmState::GoalComplete,
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

#[tokio::test(flavor = "multi_thread")]
async fn test_fsm_goal_timeout_triggers_goal_complete() {
    let dir = TestDir::new("loopr-fsm-goaltimeout");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI".into());
    wi.force_status(WorkStatus::InProgress);
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    // Goal started 5 hours ago, timeout is 4 hours
    coord_state.goal_started_at = crate::id::now_millis() - 18_000_000;

    let config = CoordinatorConfig {
        goal_timeout_secs: 14400, // 4 hours
        ..CoordinatorConfig::default()
    };

    let result = check_fsm_transition(&stores, &coord_state, &config);
    assert_eq!(result, Some(CoordinatorFsmState::GoalComplete));
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

// build_phase_status failure learnings test removed - build_phase_status no longer exists.

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

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Insert a Work directly at Integrated status
    let mut wi = Work::new(phase_id.clone(), "WI 1".into());
    wi.force_status(WorkStatus::Integrated);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx,
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

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

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Insert a Work at Ready (not Integrated)
    let wi = Work::new(phase_id.clone(), "WI 1".into());
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx,
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

    sweep_integrated_to_done(&stores, &coord_state, &bridge, TEST_PREFIX);

    // Work should still be Draft (unchanged by sweep)
    let works = stores.works.read().unwrap();
    let unchanged = works.get(&wi_id).unwrap();
    assert_eq!(unchanged.status(), WorkStatus::Draft);
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

// --- Phase 1: quality gate tests (goal_abandon_ratio_terminal + check_abandon_gate) ---

#[tokio::test(flavor = "multi_thread")]
async fn test_goal_abandon_ratio_terminal_excludes_non_terminal() {
    let dir = TestDir::new("loopr-coord-gate-ratio");
    let stores = test_stores(&dir);

    let plan_id = "plan-1".to_string();

    // 5 terminal (3 Done + 2 Abandoned) + 1 non-terminal (InProgress)
    // Terminal-only ratio: 2/5 = 40% (not > 40%, so gate should NOT fire)
    // All-works ratio: 2/6 = 33% (different)
    for i in 0..3 {
        let mut w = Work::new(plan_id.clone(), format!("Done {}", i));
        w.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }
    for i in 0..2 {
        let mut w = Work::new(plan_id.clone(), format!("Abandoned {}", i));
        w.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }
    let mut w_in_progress = Work::new(plan_id.clone(), "InProgress".into());
    w_in_progress.force_status(WorkStatus::InProgress);
    stores
        .works
        .write()
        .unwrap()
        .insert(w_in_progress.id.clone(), w_in_progress);

    let ratio = crate::agents::generation::goal_abandon_ratio_terminal(&stores, &plan_id);
    // 2 abandoned / 5 terminal = 0.4
    assert!(
        (ratio - 0.4).abs() < 1e-6,
        "terminal-only ratio should be 0.4, got {}",
        ratio
    );

    // All-works ratio should be different (2/6 ≈ 0.333)
    let all_ratio = crate::agents::generation::goal_abandon_ratio(&stores, &plan_id);
    assert!(
        (all_ratio - 2.0 / 6.0).abs() < 1e-6,
        "all-works ratio should be ~0.333, got {}",
        all_ratio
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_goal_abandon_ratio_terminal_empty_returns_zero() {
    let dir = TestDir::new("loopr-coord-gate-empty");
    let stores = test_stores(&dir);
    let ratio = crate::agents::generation::goal_abandon_ratio_terminal(&stores, "plan-none");
    assert_eq!(ratio, 0.0, "empty stores should return 0.0");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_abandon_gate_fires_above_threshold() {
    // 5 abandoned / 12 terminal = 41.7% > 40% -> NeedHelp
    let dir = TestDir::new("loopr-coord-gate-fires");
    let stores = test_stores(&dir);

    let plan_id = "plan-1".to_string();

    // Insert a goal so we can look it up
    let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("test goal".to_string());
    let goal_id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

    let mut coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;
    // Override goal_id to match the plan_id where we put works
    coord_state.goal_id = plan_id.clone();

    // 7 Done + 5 Abandoned = 12 terminal, 5/12 ≈ 41.7% > 40%
    for i in 0..7 {
        let mut w = Work::new(plan_id.clone(), format!("Done {}", i));
        w.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }
    for i in 0..5 {
        let mut w = Work::new(plan_id.clone(), format!("Abandoned {}", i));
        w.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }

    let outcome = check_abandon_gate(&stores, &coord_state, TEST_PREFIX);
    assert!(outcome.is_some(), "gate should fire when ratio > threshold");
    match outcome.unwrap() {
        IterationOutcome::NeedHelp(reason) => {
            assert!(
                reason.contains("Quality gate"),
                "reason should mention quality gate: {reason}"
            );
            assert!(reason.contains("5/12"), "reason should include counts: {reason}");
        }
        other => panic!("expected NeedHelp, got {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_abandon_gate_passes_below_threshold() {
    // 2 abandoned / 5 terminal = 40% (not strictly greater than 40%) -> None
    let dir = TestDir::new("loopr-coord-gate-pass");
    let stores = test_stores(&dir);

    let plan_id = "plan-pass".to_string();

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.goal_id = plan_id.clone();

    for i in 0..3 {
        let mut w = Work::new(plan_id.clone(), format!("Done {}", i));
        w.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }
    for i in 0..2 {
        let mut w = Work::new(plan_id.clone(), format!("Abandoned {}", i));
        w.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }

    let outcome = check_abandon_gate(&stores, &coord_state, TEST_PREFIX);
    assert!(
        outcome.is_none(),
        "gate should not fire at exactly 40% (not strictly greater)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_apply_fsm_goal_complete_fires_quality_gate() {
    // When apply_fsm_transition targets GoalComplete with >40% abandoned terminal works,
    // it should return NeedHelp (not Done) and NOT deactivate the goal.
    let dir = TestDir::new("loopr-coord-qgate-fires");
    let stores = test_stores(&dir);

    let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("test goal".to_string());
    let goal_id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

    let mut coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    // 5 abandoned / 12 terminal = 41.7% > 40%: same plan_id as goal_id
    for i in 0..7 {
        let mut w = Work::new(goal_id.clone(), format!("Done {}", i));
        w.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }
    for i in 0..5 {
        let mut w = Work::new(goal_id.clone(), format!("Abandoned {}", i));
        w.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }

    let outcome = apply_fsm_transition(
        CoordinatorFsmState::GoalComplete,
        &mut coord_state,
        &stores,
        TEST_PREFIX,
    );

    assert!(outcome.is_some(), "should return an outcome");
    match outcome.unwrap() {
        IterationOutcome::NeedHelp(reason) => {
            assert!(reason.contains("Quality gate"), "got: {reason}");
        }
        other => panic!("expected NeedHelp, got {:?}", other),
    }

    // Goal must NOT be deactivated when gate fires
    let goals = stores.coordinator_goals.read().unwrap();
    let goal = goals.get(&goal_id).unwrap();
    assert!(goal.active, "goal must remain active when quality gate fires");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_apply_fsm_goal_complete_passes_quality_gate() {
    // When ratio is below threshold, apply_fsm_transition should return Done and deactivate goal.
    let dir = TestDir::new("loopr-coord-qgate-pass");
    let stores = test_stores(&dir);

    let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("test goal".to_string());
    let goal_id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

    let mut coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    // 2 abandoned / 5 terminal = 40% (not > 40%): gate passes
    for i in 0..3 {
        let mut w = Work::new(goal_id.clone(), format!("Done {}", i));
        w.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }
    for i in 0..2 {
        let mut w = Work::new(goal_id.clone(), format!("Abandoned {}", i));
        w.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(w.id.clone(), w);
    }

    let outcome = apply_fsm_transition(
        CoordinatorFsmState::GoalComplete,
        &mut coord_state,
        &stores,
        TEST_PREFIX,
    );

    assert!(outcome.is_some(), "should return an outcome");
    match outcome.unwrap() {
        IterationOutcome::Done(_) => {}
        other => panic!("expected Done, got {:?}", other),
    }

    // Goal must be deactivated when gate passes
    let goals = stores.coordinator_goals.read().unwrap();
    let goal = goals.get(&goal_id).unwrap();
    assert!(!goal.active, "goal must be deactivated when gate passes");
}

// --- Phase 1: validate_action_coherence tests (structured requires_replacement field) ---

#[test]
fn test_validate_action_coherence_no_warning_with_create_work() {
    // requires_replacement=true + create_work in same payload -> no warning
    let actions = vec![
        AgentAction::OverrideWork {
            work_id: "wi-1".into(),
            target_status: "Superseded".into(),
            reason: "Replacing with better-scoped work".into(),
            requires_replacement: true,
        },
        AgentAction::CreateWork {
            parent_id: "phase-1".into(),
            title: "Replacement".into(),
            description: "fixed".into(),
            files: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        },
    ];
    let warnings = validate_action_coherence(&actions, TEST_PREFIX);
    assert!(
        warnings.is_empty(),
        "no warnings expected when create_work accompanies override_work with requires_replacement=true"
    );
}

#[test]
fn test_validate_action_coherence_warns_when_requires_replacement_without_create() {
    // requires_replacement=true but no create_work -> warning
    let actions = vec![AgentAction::OverrideWork {
        work_id: "wi-1".into(),
        target_status: "Superseded".into(),
        reason: "Superseding this work".into(),
        requires_replacement: true,
    }];
    let warnings = validate_action_coherence(&actions, TEST_PREFIX);
    assert_eq!(
        warnings.len(),
        1,
        "should warn when requires_replacement=true but no create_work present"
    );
    assert!(warnings[0].contains("wi-1"), "warning should mention the work ID");
    assert!(
        warnings[0].contains("requires_replacement=true"),
        "warning should mention the field"
    );
}

#[test]
fn test_validate_action_coherence_no_warning_clean_abandon() {
    // requires_replacement=false (clean abandon) -> no warning regardless of reason text
    let actions = vec![AgentAction::OverrideWork {
        work_id: "wi-2".into(),
        target_status: "Abandoned".into(),
        reason: "Blocked by unresolvable dependency - creating new approach".into(),
        requires_replacement: false,
    }];
    let warnings = validate_action_coherence(&actions, TEST_PREFIX);
    assert!(warnings.is_empty(), "requires_replacement=false never warns");
}

#[test]
fn test_validate_action_coherence_default_false_no_warning() {
    // requires_replacement defaults to false -> no warning even with creating-sounding reason
    let actions = vec![AgentAction::OverrideWork {
        work_id: "wi-3".into(),
        target_status: "Abandoned".into(),
        reason: "Creating a fresh approach to this problem".into(),
        requires_replacement: false,
    }];
    let warnings = validate_action_coherence(&actions, TEST_PREFIX);
    assert!(
        warnings.is_empty(),
        "requires_replacement=false is always coherent, regardless of reason text"
    );
}

#[test]
fn test_validate_action_coherence_requires_replacement_ready_override_no_create() {
    // requires_replacement=true on Ready override without create_work -> warning
    // (coherence gate applies to any target_status, not just Superseded)
    let actions = vec![AgentAction::OverrideWork {
        work_id: "wi-4".into(),
        target_status: "Ready".into(),
        reason: "Resetting for retry".into(),
        requires_replacement: true,
    }];
    let warnings = validate_action_coherence(&actions, TEST_PREFIX);
    assert_eq!(
        warnings.len(),
        1,
        "requires_replacement=true always requires create_work regardless of target_status"
    );
}

#[test]
fn test_validate_action_coherence_empty_actions() {
    let warnings = validate_action_coherence(&[], TEST_PREFIX);
    assert!(warnings.is_empty(), "empty action list should never warn");
}

// --- coherence enforcement run_iteration tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_run_iteration_incoherent_actions_returns_continue_with_feedback() {
    // override_work with "Creating replacement" but no create_work -> coherence gate
    // should return Continue with feedback, not execute the actions.
    crate::prompts::init_defaults();
    let dir = TestDir::new("loopr-coord-coherence1");
    let stores = test_stores(&dir);

    let agent = test_coordinator(
        &dir,
        &stores,
        vec![
            r#"[{"action": "override_work", "work_id": "wi-1", "target_status": "Superseded", "reason": "Replacing this work", "requires_replacement": true}]"#
                .to_string(),
        ],
        CoordinatorConfig::default(),
    );

    let mut coord_state = CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive);
    let mut guard = Lifeguard::new();
    let result = agent.run_iteration(&mut coord_state, &mut guard).await;

    assert!(result.is_ok());
    match result.unwrap() {
        IterationOutcome::Continue(feedback) => {
            assert!(
                feedback.contains("incoherent"),
                "feedback should mention incoherence, got: {}",
                feedback
            );
        }
        other => panic!("expected Continue with feedback, got: {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_run_iteration_coherent_requires_replacement_false_executes_normally() {
    // override_work with requires_replacement=false and no create_work -> coherent
    // (requires_replacement=false means no replacement is expected, so no gate fires).
    crate::prompts::init_defaults();
    let dir = TestDir::new("loopr-coord-coherence2");
    let stores = test_stores(&dir);

    let agent = test_coordinator(
        &dir,
        &stores,
        vec![
            r#"[{"action": "override_work", "work_id": "wi-1", "target_status": "Abandoned", "reason": "Feature descoped, no replacement", "requires_replacement": false}]"#
                .to_string(),
        ],
        CoordinatorConfig::default(),
    );

    let mut coord_state = CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive);
    let mut guard = Lifeguard::new();
    let result = agent.run_iteration(&mut coord_state, &mut guard).await;

    assert!(result.is_ok());
    // Should NOT be Continue with coherence feedback - requires_replacement=false passes the gate.
    // The override_work action will fail (no matching work in stores), but that's an action
    // execution failure, not a coherence failure.
    match result.unwrap() {
        IterationOutcome::Continue(feedback) => {
            assert!(
                !feedback.contains("incoherent"),
                "requires_replacement=false should pass coherence gate, but got: {}",
                feedback
            );
        }
        _ => {} // Done or NeedHelp are also acceptable - the key is it wasn't a coherence rejection
    }
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

// --- Prefix validation: reconciler Layer 3 defense-in-depth ---

#[tokio::test(flavor = "multi_thread")]
async fn test_reconciler_cross_type_dep_blocks_spec_promotion() {
    // A Spec with a dep on a Phase ID (ph-*) must NOT be promoted to Active.
    // all_hierarchy_deps_terminal must reject the cross-type dep and return false.
    let dir = TestDir::new("loopr-reconcile-crosstype-spec");
    let stores = test_stores(&dir);

    // Create an Active Plan
    let mut plan = Plan::new("Goal".to_string(), crate::domain::criteria::AcceptanceCriteria(vec![]));
    plan.force_status(crate::domain::plan::PlanStatus::Active);
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    // Create a Spec with a cross-type dep (a Phase ID)
    let mut spec = Spec::new(plan_id.clone(), "My Spec".into());
    spec.force_status(crate::domain::spec::SpecStatus::Pending);
    let phase_id = crate::id::generate_id("ph");
    spec.dependencies = vec![phase_id.clone()];
    let spec_id = spec.id.clone();
    stores.write_specs().unwrap().insert(spec_id.clone(), spec);

    let outcome = reconcile::reconcile(&stores);

    assert_eq!(outcome.promoted, 0, "cross-type dep should block Spec promotion");

    let specs = stores.read_specs().unwrap();
    assert_eq!(
        specs.get(&spec_id).unwrap().status(),
        crate::domain::spec::SpecStatus::Pending,
        "Spec should remain Pending when cross-type dep is present"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reconciler_same_type_dep_promotes_spec() {
    // A Spec with a dep on a Complete Spec (sp-*) must be promoted to Active.
    let dir = TestDir::new("loopr-reconcile-sametype-spec");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Goal".to_string(), crate::domain::criteria::AcceptanceCriteria(vec![]));
    plan.force_status(crate::domain::plan::PlanStatus::Active);
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    // Create dep Spec in Complete state
    let mut dep_spec = Spec::new(plan_id.clone(), "Dep Spec".into());
    dep_spec.force_status(crate::domain::spec::SpecStatus::Complete);
    let dep_spec_id = dep_spec.id.clone();
    stores.write_specs().unwrap().insert(dep_spec_id.clone(), dep_spec);

    // Create the Spec under test with dep on a Complete Spec
    let mut spec = Spec::new(plan_id.clone(), "Dependent Spec".into());
    spec.force_status(crate::domain::spec::SpecStatus::Pending);
    spec.dependencies = vec![dep_spec_id.clone()];
    let spec_id = spec.id.clone();
    stores.write_specs().unwrap().insert(spec_id.clone(), spec);

    let outcome = reconcile::reconcile(&stores);

    assert!(
        outcome.promoted > 0,
        "same-type complete dep should allow Spec promotion"
    );

    let specs = stores.read_specs().unwrap();
    let final_status = specs.get(&spec_id).unwrap().status();
    assert_ne!(
        final_status,
        crate::domain::spec::SpecStatus::Pending,
        "Spec should not remain Pending when dep is Complete (got {:?})",
        final_status
    );
}

#[test]
fn test_append_work_status_includes_bundle_id_for_inreview_work() {
    use crate::domain::bundle::BundleStatus;
    use crate::domain::coordinator_state::CoordinatorState;
    use crate::domain::work::Work;
    use std::collections::HashMap;

    let mut wi = Work::new("phase-1".into(), "update_bookmark".into());
    wi.force_status(WorkStatus::InReview);
    let wi_id = wi.id.clone();

    let mut bundle = Bundle::new(wi_id.clone(), None, "branch-a".into(), vec![]);
    bundle.force_status(BundleStatus::Triaged);
    let bundle_id = bundle.id.clone();

    let mut all_works = HashMap::new();
    all_works.insert(wi_id.clone(), wi.clone());

    let mut bundles = HashMap::new();
    bundles.insert(bundle_id.clone(), bundle);

    let coord_state = CoordinatorState::new("goal-1".into(), crate::config::InterviewMode::default());
    let phase_works: Vec<_> = all_works.values().collect();

    let mut summary = String::new();
    let fsm = crate::fsm::runtime::FsmInterpreter::embedded().unwrap();
    append_work_status(&mut summary, &phase_works, &coord_state, &all_works, &bundles, &fsm);

    assert!(
        summary.contains(&bundle_id),
        "bundle ID should appear in coordinator context for InReview work, got: {}",
        summary
    );
    assert!(
        summary.contains("Triaged"),
        "bundle status should appear in coordinator context, got: {}",
        summary
    );
}

// --- sweep_stuck_inreview tests (Fix 4) ---

/// Verify that sweep_stuck_inreview advances an InReview work when all bundles are terminal
/// and at least one is Merged.
#[tokio::test(flavor = "multi_thread")]
async fn test_sweep_stuck_inreview_with_merged_bundle() {
    let dir = TestDir::new("loopr-coord-inreview-merged");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    // Work stuck in InReview
    let mut wi = Work::new(phase_id.clone(), "WI stuck".into());
    wi.force_status(WorkStatus::InReview);
    wi.acceptance_criteria = crate::domain::criteria::AcceptanceCriteria(vec!["tests pass".into()]);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    // Bundle in terminal Merged state
    let mut bundle = Bundle::new(wi_id.clone(), None, String::new(), vec!["claim".into()]);
    bundle.force_status(BundleStatus::Merged);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx,
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

    sweep_stuck_inreview(&stores, &coord_state, &bridge, TEST_PREFIX);

    // Work should now be Integrated
    let works = stores.works.read().unwrap();
    assert_eq!(
        works.get(&wi_id).unwrap().status(),
        WorkStatus::Integrated,
        "stuck InReview work with Merged bundle should advance to Integrated"
    );
}

/// Verify that sweep_stuck_inreview does NOT fire when bundles are not all terminal.
#[tokio::test(flavor = "multi_thread")]
async fn test_sweep_inreview_not_all_terminal() {
    let dir = TestDir::new("loopr-coord-inreview-partial");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI partial".into());
    wi.force_status(WorkStatus::InReview);
    wi.acceptance_criteria = crate::domain::criteria::AcceptanceCriteria(vec!["tests pass".into()]);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    // One bundle Merged, one still Proposed (not terminal)
    let mut merged = Bundle::new(wi_id.clone(), None, String::new(), vec!["claim".into()]);
    merged.force_status(BundleStatus::Merged);
    stores.bundles.write().unwrap().insert(merged.id.clone(), merged);

    let active = Bundle::new(wi_id.clone(), None, "agent/wi-1".into(), vec!["claim2".into()]);
    // Default status is Proposed (non-terminal)
    stores.bundles.write().unwrap().insert(active.id.clone(), active);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx,
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

    sweep_stuck_inreview(&stores, &coord_state, &bridge, TEST_PREFIX);

    // Work should still be InReview (not all bundles terminal)
    let works = stores.works.read().unwrap();
    assert_eq!(
        works.get(&wi_id).unwrap().status(),
        WorkStatus::InReview,
        "should not advance when not all bundles are terminal"
    );
}

/// Verify that sweep_stuck_inreview does NOT fire when all bundles are Rejected (no Merged).
#[tokio::test(flavor = "multi_thread")]
async fn test_sweep_inreview_no_merged_bundle() {
    let dir = TestDir::new("loopr-coord-inreview-nomerge");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase_id.clone(), phase);

    let mut wi = Work::new(phase_id.clone(), "WI all rejected".into());
    wi.force_status(WorkStatus::InReview);
    wi.acceptance_criteria = crate::domain::criteria::AcceptanceCriteria(vec!["tests pass".into()]);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi_id.clone(), wi);

    // All bundles Rejected (terminal but no Merged)
    let mut rejected = Bundle::new(wi_id.clone(), None, "agent/wi-1".into(), vec!["claim".into()]);
    rejected.force_status(BundleStatus::Rejected);
    stores.bundles.write().unwrap().insert(rejected.id.clone(), rejected);

    let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
    coord_state.fsm_state = CoordinatorFsmState::Executing;

    let (event_tx, _rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx,
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

    sweep_stuck_inreview(&stores, &coord_state, &bridge, TEST_PREFIX);

    // Work should still be InReview (no Merged bundle, must go through rejection path)
    let works = stores.works.read().unwrap();
    assert_eq!(
        works.get(&wi_id).unwrap().status(),
        WorkStatus::InReview,
        "should not advance when all bundles are Rejected"
    );
}
