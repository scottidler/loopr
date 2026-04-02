use std::fmt;

use serde::{Deserialize, Serialize};

/// The type of agent -- determines behavior and prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Implementer,
    Reviewer,
    Coordinator,
    Researcher,
    Integrator,
    Chat,
}

impl AgentType {
    /// Returns the default Role corresponding to this agent type.
    pub fn default_role(&self) -> crate::domain::role::Role {
        match self {
            AgentType::Implementer => crate::domain::role::Role::Implementer,
            AgentType::Reviewer => crate::domain::role::Role::Reviewer,
            AgentType::Coordinator => crate::domain::role::Role::Coordinator,
            AgentType::Researcher => crate::domain::role::Role::Researcher,
            AgentType::Integrator => crate::domain::role::Role::Integrator,
            AgentType::Chat => crate::domain::role::Role::Coordinator, // Chat uses Coordinator role as default
        }
    }

    /// Returns true if this agent type operates in the "thinking plane" (no worktree needed).
    pub fn is_thinking_plane(&self) -> bool {
        matches!(
            self,
            AgentType::Coordinator
                | AgentType::Researcher
                | AgentType::Integrator
                | AgentType::Reviewer
                | AgentType::Chat
        )
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentType::Implementer => write!(f, "implementer"),
            AgentType::Reviewer => write!(f, "reviewer"),
            AgentType::Coordinator => write!(f, "coordinator"),
            AgentType::Researcher => write!(f, "researcher"),
            AgentType::Integrator => write!(f, "integrator"),
            AgentType::Chat => write!(f, "chat"),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    const ALL_AGENT_TYPES: [AgentType; 5] = [
        AgentType::Implementer,
        AgentType::Reviewer,
        AgentType::Coordinator,
        AgentType::Researcher,
        AgentType::Integrator,
    ];

    #[test]
    fn test_agent_type_display() {
        assert_eq!(AgentType::Implementer.to_string(), "implementer");
        assert_eq!(AgentType::Reviewer.to_string(), "reviewer");
        assert_eq!(AgentType::Coordinator.to_string(), "coordinator");
        assert_eq!(AgentType::Researcher.to_string(), "researcher");
        assert_eq!(AgentType::Integrator.to_string(), "integrator");
    }

    #[test]
    fn test_agent_type_serde_roundtrip() {
        for at in ALL_AGENT_TYPES {
            let json = serde_json::to_string(&at).unwrap();
            let deserialized: AgentType = serde_json::from_str(&json).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_display_matches_serde() {
        for at in ALL_AGENT_TYPES {
            let display = at.to_string();
            let quoted = format!("\"{display}\"");
            let deserialized: AgentType = serde_json::from_str(&quoted).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_default_role() {
        use crate::domain::role::Role;
        assert_eq!(AgentType::Implementer.default_role(), Role::Implementer);
        assert_eq!(AgentType::Reviewer.default_role(), Role::Reviewer);
        assert_eq!(AgentType::Coordinator.default_role(), Role::Coordinator);
        assert_eq!(AgentType::Researcher.default_role(), Role::Researcher);
        assert_eq!(AgentType::Integrator.default_role(), Role::Integrator);
    }

    #[test]
    fn test_agent_type_is_thinking_plane() {
        assert!(!AgentType::Implementer.is_thinking_plane());
        assert!(AgentType::Reviewer.is_thinking_plane());
        assert!(AgentType::Coordinator.is_thinking_plane());
        assert!(AgentType::Researcher.is_thinking_plane());
        assert!(AgentType::Integrator.is_thinking_plane());
    }
}
