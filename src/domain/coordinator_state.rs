use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::config::InterviewMode;
use crate::id;

/// A single interview exchange: questions asked and user's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterviewExchange {
    /// Questions asked in this round
    pub questions: Vec<String>,
    /// User's response to this round's questions
    pub answer: String,
    pub timestamp: i64,
}

/// FSM states for the Coordinator control loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordinatorFsmState {
    Interviewing,
    /// Waiting for background decomposition to complete before advancing to Planning.
    Decomposing,
    Planning,
    /// Reconciliation loop runs here - promotes Pending records, detects completions.
    Executing,
    GoalComplete,
}

impl CoordinatorFsmState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, CoordinatorFsmState::GoalComplete)
    }
}

impl std::fmt::Display for CoordinatorFsmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoordinatorFsmState::Decomposing => write!(f, "Decomposing"),
            other => write!(f, "{:?}", other),
        }
    }
}

/// Persistent state for the Coordinator's reactive execution loop.
/// Persisted in TaskStore so the Coordinator survives daemon restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorState {
    pub id: String,
    pub goal_id: String,
    pub fsm_state: CoordinatorFsmState,
    /// Legacy field - kept for serde backward compat, always None in new code.
    #[serde(default)]
    pub current_phase_id: Option<String>,
    pub work_attempts: HashMap<String, u32>,
    /// Wall-clock timestamp (millis since epoch) when each Work was first assigned.
    /// Used for SLA breach detection. NOT overwritten on re-assignment.
    #[serde(default)]
    pub work_first_assigned_at: HashMap<String, i64>,
    /// Legacy field - kept for serde backward compat, always None in new code.
    #[serde(default)]
    pub phase_activated_at: Option<i64>,
    /// Error message from the most recent decomposition failure. When set, the coordinator
    /// transitions to NeedsHelp instead of busy-polling. Cleared on re-decompose.
    #[serde(default)]
    pub decomposition_error: Option<String>,
    /// Decomposition attempt count per parent ID.
    /// Tracks how many times coverage evaluation has failed for children of a parent.
    #[serde(default)]
    pub decomposition_attempts: HashMap<String, u32>,
    /// Number of times we've bubbled up (revised a parent due to child decomposition failure).
    /// Guarded by config.strategy.max_bubble_up_depth. Reset on each new goal.
    #[serde(default)]
    pub bubble_up_count: u32,
    pub goal_started_at: i64,
    /// Legacy field - kept for serde backward compat, always empty in new code.
    #[serde(default)]
    pub phases_completed: Vec<String>,
    /// Interview context accumulated during the Interviewing state.
    #[serde(default)]
    pub interview_context: Vec<InterviewExchange>,
    /// Number of researchers spawned per scope_id in the current phase.
    /// Used to enforce the spawn limit (default: 3 per scope).
    /// Reset when the phase changes.
    #[serde(default)]
    pub researcher_spawns: HashMap<String, u32>,
    /// Whether the user has approved the Plan.
    #[serde(default)]
    pub plan_approved: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl CoordinatorState {
    pub fn new(goal_id: String, interview_mode: InterviewMode) -> Self {
        tracing::debug!(
            "CoordinatorState::new(goal_id={}, interview_mode={:?})",
            goal_id,
            interview_mode
        );
        let now = id::now_millis();
        let (fsm_state, plan_approved) = match interview_mode {
            InterviewMode::Skip => (CoordinatorFsmState::Planning, true),
            InterviewMode::Auto | InterviewMode::Interactive => (CoordinatorFsmState::Interviewing, false),
        };
        Self {
            id: id::generate_id("cs"),
            goal_id,
            fsm_state,
            current_phase_id: None,
            work_attempts: HashMap::new(),
            work_first_assigned_at: HashMap::new(),
            phase_activated_at: None,
            decomposition_error: None,
            decomposition_attempts: HashMap::new(),
            bubble_up_count: 0,
            researcher_spawns: HashMap::new(),
            goal_started_at: now,
            phases_completed: Vec::new(),
            interview_context: Vec::new(),
            plan_approved,
            created_at: now,
            updated_at: now,
        }
    }

    /// Transition to a new FSM state.
    pub fn transition_to(&mut self, new_state: CoordinatorFsmState) {
        tracing::debug!("CoordinatorState::transition_to(id={}, target={})", self.id, new_state);
        self.fsm_state = new_state;
        self.updated_at = id::now_millis();
    }

    /// Increment the attempt counter for a work. Returns the new count.
    pub fn increment_attempts(&mut self, work_id: &str) -> u32 {
        let count = self.work_attempts.entry(work_id.to_string()).or_insert(0);
        *count += 1;
        self.updated_at = id::now_millis();
        *count
    }

    /// Get the attempt count for a work.
    pub fn attempts(&self, work_id: &str) -> u32 {
        self.work_attempts.get(work_id).copied().unwrap_or(0)
    }

    /// Record the first assignment time for a Work. Only sets the timestamp
    /// if no entry exists — preserves the original SLA start across re-assignments.
    pub fn record_first_assignment(&mut self, work_id: &str) {
        self.work_first_assigned_at
            .entry(work_id.to_string())
            .or_insert_with(id::now_millis);
        self.updated_at = id::now_millis();
    }

    /// Increment the decomposition attempt counter for a parent. Returns the new count.
    pub fn increment_decomposition_attempts(&mut self, parent_id: &str) -> u32 {
        let count = self.decomposition_attempts.entry(parent_id.to_string()).or_insert(0);
        *count += 1;
        self.updated_at = id::now_millis();
        *count
    }

    /// Get the decomposition attempt count for a parent.
    pub fn decomposition_attempts(&self, parent_id: &str) -> u32 {
        self.decomposition_attempts.get(parent_id).copied().unwrap_or(0)
    }

    /// Reset decomposition attempts for a parent (after bubble-up).
    pub fn reset_decomposition_attempts(&mut self, parent_id: &str) {
        self.decomposition_attempts.remove(parent_id);
        self.updated_at = id::now_millis();
    }

    /// Increment the bubble-up counter. Returns the new count.
    pub fn increment_bubble_up(&mut self) -> u32 {
        self.bubble_up_count += 1;
        self.updated_at = id::now_millis();
        self.bubble_up_count
    }

    /// Reset the bubble-up counter (called on new goal).
    pub fn reset_bubble_up(&mut self) {
        self.bubble_up_count = 0;
        self.updated_at = id::now_millis();
    }

    /// Increment the researcher spawn counter for a scope. Returns the new count.
    pub fn increment_researcher_spawns(&mut self, scope_id: &str) -> u32 {
        let count = self.researcher_spawns.entry(scope_id.to_string()).or_insert(0);
        *count += 1;
        self.updated_at = id::now_millis();
        *count
    }

    /// Get the researcher spawn count for a scope.
    pub fn researcher_spawn_count(&self, scope_id: &str) -> u32 {
        self.researcher_spawns.get(scope_id).copied().unwrap_or(0)
    }

    /// Get the wall-clock age in minutes since first assignment, or None if never assigned.
    pub fn work_age_minutes(&self, work_id: &str, now: i64) -> Option<i64> {
        self.work_first_assigned_at
            .get(work_id)
            .map(|&started| (now - started) / 60_000)
    }
}

impl Record for CoordinatorState {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "coordinator_states"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("goal_id".into(), IndexValue::String(self.goal_id.clone()));
        m.insert("fsm_state".into(), IndexValue::String(self.fsm_state.to_string()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_state_new() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert!(!state.id.is_empty());
        assert_eq!(state.goal_id, "goal-1");
        assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);
        assert!(state.current_phase_id.is_none());
        assert!(state.work_attempts.is_empty());
        assert!(state.phase_activated_at.is_none());
        assert!(state.phases_completed.is_empty());
        assert!(state.created_at > 0);
        assert_eq!(state.created_at, state.updated_at);
    }

    #[test]
    fn test_fsm_state_transitions() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);

        state.transition_to(CoordinatorFsmState::Planning);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);

        state.transition_to(CoordinatorFsmState::Executing);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);

        state.transition_to(CoordinatorFsmState::GoalComplete);
        assert!(state.fsm_state.is_terminal());
    }

    #[test]
    fn test_work_attempts() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.attempts("wi-1"), 0);

        assert_eq!(state.increment_attempts("wi-1"), 1);
        assert_eq!(state.increment_attempts("wi-1"), 2);
        assert_eq!(state.increment_attempts("wi-1"), 3);
        assert_eq!(state.attempts("wi-1"), 3);
        assert_eq!(state.attempts("wi-2"), 0);
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.transition_to(CoordinatorFsmState::Executing);
        state.increment_attempts("wi-1");
        state.increment_attempts("wi-1");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: CoordinatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(state.id, deserialized.id);
        assert_eq!(state.goal_id, deserialized.goal_id);
        assert_eq!(state.fsm_state, deserialized.fsm_state);
        assert_eq!(state.work_attempts, deserialized.work_attempts);
    }

    #[test]
    fn test_fsm_state_display() {
        assert_eq!(CoordinatorFsmState::Decomposing.to_string(), "Decomposing");
        assert_eq!(CoordinatorFsmState::Planning.to_string(), "Planning");
        assert_eq!(CoordinatorFsmState::Executing.to_string(), "Executing");
        assert_eq!(CoordinatorFsmState::GoalComplete.to_string(), "GoalComplete");
    }

    #[test]
    fn test_fsm_state_is_terminal() {
        assert!(!CoordinatorFsmState::Decomposing.is_terminal());
        assert!(!CoordinatorFsmState::Planning.is_terminal());
        assert!(!CoordinatorFsmState::Executing.is_terminal());
        assert!(CoordinatorFsmState::GoalComplete.is_terminal());
    }

    #[test]
    fn test_fsm_state_decomposing_serde_roundtrip() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Skip);
        state.fsm_state = CoordinatorFsmState::Decomposing;
        let json = serde_json::to_string(&state).unwrap();
        let restored: CoordinatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.fsm_state, CoordinatorFsmState::Decomposing);
        assert!(!restored.fsm_state.is_terminal());
    }

    #[test]
    fn test_record_trait() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(Record::id(&state), state.id);
        assert_eq!(Record::updated_at(&state), state.updated_at);
        assert_eq!(CoordinatorState::collection_name(), "coordinator_states");

        let fields = state.indexed_fields();
        assert_eq!(fields.get("goal_id"), Some(&IndexValue::String("goal-1".to_string())));
        assert_eq!(
            fields.get("fsm_state"),
            Some(&IndexValue::String("Interviewing".to_string()))
        );
    }

    // --- SLA tracking tests ---

    #[test]
    fn test_work_first_assigned_at_empty_on_new() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert!(state.work_first_assigned_at.is_empty());
    }

    #[test]
    fn test_record_first_assignment() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.record_first_assignment("wi-1");
        assert!(state.work_first_assigned_at.contains_key("wi-1"));
        assert!(*state.work_first_assigned_at.get("wi-1").unwrap() > 0);
    }

    #[test]
    fn test_record_first_assignment_not_overwritten() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.record_first_assignment("wi-1");
        let first_time = *state.work_first_assigned_at.get("wi-1").unwrap();
        // Simulate time passing
        std::thread::sleep(std::time::Duration::from_millis(5));
        state.record_first_assignment("wi-1");
        let second_time = *state.work_first_assigned_at.get("wi-1").unwrap();
        assert_eq!(first_time, second_time, "first assignment time must not be overwritten");
    }

    #[test]
    fn test_work_age_minutes_none_for_unassigned() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let now = crate::id::now_millis();
        assert!(state.work_age_minutes("wi-1", now).is_none());
    }

    #[test]
    fn test_work_age_minutes_zero_for_just_assigned() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.record_first_assignment("wi-1");
        let now = crate::id::now_millis();
        let age = state.work_age_minutes("wi-1", now).unwrap();
        assert!(
            (0..=1).contains(&age),
            "just-assigned work should be ~0 min old, got {}",
            age
        );
    }

    #[test]
    fn test_work_age_minutes_synthetic() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        // Manually set assigned_at to 45 minutes ago
        let now = crate::id::now_millis();
        state
            .work_first_assigned_at
            .insert("wi-1".to_string(), now - 45 * 60_000);
        let age = state.work_age_minutes("wi-1", now).unwrap();
        assert_eq!(age, 45);
    }

    #[test]
    fn test_serde_roundtrip_with_first_assigned() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.record_first_assignment("wi-1");
        state.record_first_assignment("wi-2");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: CoordinatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(state.work_first_assigned_at, deserialized.work_first_assigned_at);
    }

    #[test]
    fn test_serde_backward_compat_without_first_assigned() {
        // Old JSON without work_first_assigned_at should deserialize with empty HashMap
        let json = serde_json::json!({
            "id": "test-id",
            "goal_id": "goal-1",
            "fsm_state": "Planning",
            "current_phase_id": null,
            "work_attempts": {},
            "phase_activated_at": null,
            "goal_started_at": 1000,
            "phases_completed": [],
            "created_at": 1000,
            "updated_at": 1000
        });
        let state: CoordinatorState = serde_json::from_value(json).unwrap();
        assert!(state.work_first_assigned_at.is_empty());
    }

    // --- InterviewMode tests ---

    // --- Bubble-up tracking tests ---

    #[test]
    fn test_bubble_up_count_starts_at_zero() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.bubble_up_count, 0);
    }

    #[test]
    fn test_increment_bubble_up() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.increment_bubble_up(), 1);
        assert_eq!(state.increment_bubble_up(), 2);
        assert_eq!(state.increment_bubble_up(), 3);
        assert_eq!(state.bubble_up_count, 3);
    }

    #[test]
    fn test_reset_bubble_up() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.increment_bubble_up();
        state.increment_bubble_up();
        assert_eq!(state.bubble_up_count, 2);
        state.reset_bubble_up();
        assert_eq!(state.bubble_up_count, 0);
    }

    #[test]
    fn test_serde_backward_compat_without_bubble_up() {
        // Old JSON without bubble_up_count should deserialize with default 0
        let json = serde_json::json!({
            "id": "test-id",
            "goal_id": "goal-1",
            "fsm_state": "Planning",
            "current_phase_id": null,
            "work_attempts": {},
            "phase_activated_at": null,
            "goal_started_at": 1000,
            "phases_completed": [],
            "created_at": 1000,
            "updated_at": 1000
        });
        let state: CoordinatorState = serde_json::from_value(json).unwrap();
        assert_eq!(state.bubble_up_count, 0);
    }

    #[test]
    fn test_new_with_skip_starts_in_planning() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Skip);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);
        assert!(state.plan_approved);
    }

    #[test]
    fn test_new_with_interactive_starts_in_interviewing() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);
        assert!(!state.plan_approved);
    }

    #[test]
    fn test_new_with_auto_starts_in_interviewing() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Auto);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);
        assert!(!state.plan_approved);
    }

    // --- Researcher spawn tracking tests ---

    #[test]
    fn test_researcher_spawns_empty_on_new() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert!(state.researcher_spawns.is_empty());
    }

    #[test]
    fn test_increment_researcher_spawns() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.increment_researcher_spawns("scope-1"), 1);
        assert_eq!(state.increment_researcher_spawns("scope-1"), 2);
        assert_eq!(state.increment_researcher_spawns("scope-1"), 3);
        assert_eq!(state.researcher_spawn_count("scope-1"), 3);
    }

    #[test]
    fn test_researcher_spawn_count_unknown_scope() {
        let state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(state.researcher_spawn_count("nonexistent"), 0);
    }

    // Note: activate_phase_clears_researcher_spawns test removed - cursor model eliminated.

    #[test]
    fn test_serde_roundtrip_with_researcher_spawns() {
        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.increment_researcher_spawns("scope-1");
        state.increment_researcher_spawns("scope-1");
        state.increment_researcher_spawns("scope-2");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: CoordinatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(state.researcher_spawns, deserialized.researcher_spawns);
    }

    #[test]
    fn test_serde_backward_compat_without_researcher_spawns() {
        // Old JSON without researcher_spawns should deserialize with empty HashMap
        let json = serde_json::json!({
            "id": "test-id",
            "goal_id": "goal-1",
            "fsm_state": "Planning",
            "current_phase_id": null,
            "work_attempts": {},
            "phase_activated_at": null,
            "goal_started_at": 1000,
            "phases_completed": [],
            "created_at": 1000,
            "updated_at": 1000
        });
        let state: CoordinatorState = serde_json::from_value(json).unwrap();
        assert!(state.researcher_spawns.is_empty());
    }
}
