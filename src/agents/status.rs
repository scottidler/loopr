use std::fmt;

use serde::{Deserialize, Serialize};

/// Lifecycle status of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Starting,
    Running,
    WaitingForLlm,
    Paused,
    Idle,
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// Returns true if this is a terminal status (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Cancelled
        )
    }

    /// Validate whether a status transition is allowed.
    pub fn can_transition_to(&self, target: AgentStatus) -> bool {
        matches!(
            (self, target),
            // Starting transitions
            (AgentStatus::Starting, AgentStatus::Running)
            | (AgentStatus::Starting, AgentStatus::Failed)
            | (AgentStatus::Starting, AgentStatus::Cancelled)
            // Running transitions
            | (AgentStatus::Running, AgentStatus::WaitingForLlm)
            | (AgentStatus::Running, AgentStatus::Paused)
            | (AgentStatus::Running, AgentStatus::Idle)
            | (AgentStatus::Running, AgentStatus::Completed)
            | (AgentStatus::Running, AgentStatus::Failed)
            | (AgentStatus::Running, AgentStatus::Cancelled)
            // WaitingForLlm transitions
            | (AgentStatus::WaitingForLlm, AgentStatus::Running)
            | (AgentStatus::WaitingForLlm, AgentStatus::Failed)
            | (AgentStatus::WaitingForLlm, AgentStatus::Cancelled)
            // Paused transitions
            | (AgentStatus::Paused, AgentStatus::Running)
            | (AgentStatus::Paused, AgentStatus::Cancelled)
            // Idle transitions (Chat sessions: loop done, awaiting next input)
            | (AgentStatus::Idle, AgentStatus::Running)
            | (AgentStatus::Idle, AgentStatus::Cancelled)
        )
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentStatus::Starting => write!(f, "starting"),
            AgentStatus::Running => write!(f, "running"),
            AgentStatus::WaitingForLlm => write!(f, "waitingforllm"),
            AgentStatus::Paused => write!(f, "paused"),
            AgentStatus::Completed => write!(f, "completed"),
            AgentStatus::Failed => write!(f, "failed"),
            AgentStatus::Idle => write!(f, "idle"),
            AgentStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_status_display() {
        assert_eq!(AgentStatus::Starting.to_string(), "starting");
        assert_eq!(AgentStatus::Running.to_string(), "running");
        assert_eq!(AgentStatus::WaitingForLlm.to_string(), "waitingforllm");
        assert_eq!(AgentStatus::Paused.to_string(), "paused");
        assert_eq!(AgentStatus::Completed.to_string(), "completed");
        assert_eq!(AgentStatus::Failed.to_string(), "failed");
        assert_eq!(AgentStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_agent_status_serde_roundtrip() {
        for status in [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::WaitingForLlm,
            AgentStatus::Paused,
            AgentStatus::Completed,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: AgentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_agent_status_is_terminal() {
        assert!(!AgentStatus::Starting.is_terminal());
        assert!(!AgentStatus::Running.is_terminal());
        assert!(!AgentStatus::WaitingForLlm.is_terminal());
        assert!(!AgentStatus::Paused.is_terminal());
        assert!(AgentStatus::Completed.is_terminal());
        assert!(AgentStatus::Failed.is_terminal());
        assert!(AgentStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_agent_status_valid_transitions() {
        // Starting transitions
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Cancelled));

        // Running transitions
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::WaitingForLlm));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Paused));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Completed));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Cancelled));

        // WaitingForLlm transitions
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Cancelled));

        // Paused transitions
        assert!(AgentStatus::Paused.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::Paused.can_transition_to(AgentStatus::Cancelled));
    }

    #[test]
    fn test_agent_status_invalid_transitions() {
        // Terminal states cannot transition
        assert!(!AgentStatus::Completed.can_transition_to(AgentStatus::Running));
        assert!(!AgentStatus::Failed.can_transition_to(AgentStatus::Running));
        assert!(!AgentStatus::Cancelled.can_transition_to(AgentStatus::Running));

        // Cannot skip states
        assert!(!AgentStatus::Starting.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Starting.can_transition_to(AgentStatus::Paused));
        assert!(!AgentStatus::Paused.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Paused.can_transition_to(AgentStatus::WaitingForLlm));
    }
}
