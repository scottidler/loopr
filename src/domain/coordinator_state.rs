use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::id;

/// FSM states for the Coordinator control loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordinatorFsmState {
    Planning,
    ActivatePhase,
    Executing,
    PhaseGate,
    GoalComplete,
}

impl CoordinatorFsmState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, CoordinatorFsmState::GoalComplete)
    }
}

impl std::fmt::Display for CoordinatorFsmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Persistent state for the Coordinator's phase-gated control loop.
/// Persisted in TaskStore so the Coordinator survives daemon restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorState {
    pub id: String,
    pub goal_id: String,
    pub fsm_state: CoordinatorFsmState,
    pub current_phase_id: Option<String>,
    pub work_attempts: HashMap<String, u32>,
    pub phase_activated_at: Option<i64>,
    pub goal_started_at: i64,
    pub phases_completed: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl CoordinatorState {
    pub fn new(goal_id: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            goal_id,
            fsm_state: CoordinatorFsmState::Planning,
            current_phase_id: None,
            work_attempts: HashMap::new(),
            phase_activated_at: None,
            goal_started_at: now,
            phases_completed: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Transition to a new FSM state.
    pub fn transition_to(&mut self, new_state: CoordinatorFsmState) {
        self.fsm_state = new_state;
        self.updated_at = id::now_millis();
    }

    /// Record that a phase has been activated.
    pub fn activate_phase(&mut self, phase_id: String) {
        self.current_phase_id = Some(phase_id);
        self.phase_activated_at = Some(id::now_millis());
        self.fsm_state = CoordinatorFsmState::Executing;
        self.updated_at = id::now_millis();
    }

    /// Record that the current phase has completed.
    pub fn complete_phase(&mut self) {
        if let Some(ref phase_id) = self.current_phase_id {
            self.phases_completed.push(phase_id.clone());
        }
        self.current_phase_id = None;
        self.phase_activated_at = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_state_new() {
        let state = CoordinatorState::new("goal-1".to_string());
        assert!(!state.id.is_empty());
        assert_eq!(state.goal_id, "goal-1");
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);
        assert!(state.current_phase_id.is_none());
        assert!(state.work_attempts.is_empty());
        assert!(state.phase_activated_at.is_none());
        assert!(state.phases_completed.is_empty());
        assert!(state.created_at > 0);
        assert_eq!(state.created_at, state.updated_at);
    }

    #[test]
    fn test_fsm_state_transitions() {
        let mut state = CoordinatorState::new("goal-1".to_string());
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);

        state.transition_to(CoordinatorFsmState::ActivatePhase);
        assert_eq!(state.fsm_state, CoordinatorFsmState::ActivatePhase);

        state.activate_phase("phase-1".to_string());
        assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);
        assert_eq!(state.current_phase_id.as_deref(), Some("phase-1"));
        assert!(state.phase_activated_at.is_some());

        state.transition_to(CoordinatorFsmState::PhaseGate);
        assert_eq!(state.fsm_state, CoordinatorFsmState::PhaseGate);

        state.complete_phase();
        assert!(state.current_phase_id.is_none());
        assert!(state.phase_activated_at.is_none());
        assert_eq!(state.phases_completed, vec!["phase-1"]);

        state.transition_to(CoordinatorFsmState::GoalComplete);
        assert!(state.fsm_state.is_terminal());
    }

    #[test]
    fn test_work_attempts() {
        let mut state = CoordinatorState::new("goal-1".to_string());
        assert_eq!(state.attempts("wi-1"), 0);

        assert_eq!(state.increment_attempts("wi-1"), 1);
        assert_eq!(state.increment_attempts("wi-1"), 2);
        assert_eq!(state.increment_attempts("wi-1"), 3);
        assert_eq!(state.attempts("wi-1"), 3);
        assert_eq!(state.attempts("wi-2"), 0);
    }

    #[test]
    fn test_multiple_phases_completed() {
        let mut state = CoordinatorState::new("goal-1".to_string());
        state.activate_phase("phase-1".to_string());
        state.complete_phase();
        state.activate_phase("phase-2".to_string());
        state.complete_phase();
        assert_eq!(state.phases_completed, vec!["phase-1", "phase-2"]);
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut state = CoordinatorState::new("goal-1".to_string());
        state.activate_phase("phase-1".to_string());
        state.increment_attempts("wi-1");
        state.increment_attempts("wi-1");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: CoordinatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(state.id, deserialized.id);
        assert_eq!(state.goal_id, deserialized.goal_id);
        assert_eq!(state.fsm_state, deserialized.fsm_state);
        assert_eq!(state.current_phase_id, deserialized.current_phase_id);
        assert_eq!(state.work_attempts, deserialized.work_attempts);
        assert_eq!(state.phases_completed, deserialized.phases_completed);
    }

    #[test]
    fn test_fsm_state_display() {
        assert_eq!(CoordinatorFsmState::Planning.to_string(), "Planning");
        assert_eq!(CoordinatorFsmState::ActivatePhase.to_string(), "ActivatePhase");
        assert_eq!(CoordinatorFsmState::Executing.to_string(), "Executing");
        assert_eq!(CoordinatorFsmState::PhaseGate.to_string(), "PhaseGate");
        assert_eq!(CoordinatorFsmState::GoalComplete.to_string(), "GoalComplete");
    }

    #[test]
    fn test_fsm_state_is_terminal() {
        assert!(!CoordinatorFsmState::Planning.is_terminal());
        assert!(!CoordinatorFsmState::ActivatePhase.is_terminal());
        assert!(!CoordinatorFsmState::Executing.is_terminal());
        assert!(!CoordinatorFsmState::PhaseGate.is_terminal());
        assert!(CoordinatorFsmState::GoalComplete.is_terminal());
    }

    #[test]
    fn test_record_trait() {
        let state = CoordinatorState::new("goal-1".to_string());
        assert_eq!(Record::id(&state), state.id);
        assert_eq!(Record::updated_at(&state), state.updated_at);
        assert_eq!(CoordinatorState::collection_name(), "coordinator_states");

        let fields = state.indexed_fields();
        assert_eq!(fields.get("goal_id"), Some(&IndexValue::String("goal-1".to_string())));
        assert_eq!(
            fields.get("fsm_state"),
            Some(&IndexValue::String("Planning".to_string()))
        );
    }
}
