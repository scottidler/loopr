use std::fmt;

use serde::{Deserialize, Serialize};

/// The type of agent -- determines behavior and prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Implementer,
    Reviewer,
    Coordinator,
    Researcher,
    Integrator,
    Chat,
    Decomposer,
}

impl AgentKind {
    /// Returns the default Role corresponding to this agent type.
    pub fn default_role(&self) -> crate::domain::role::Role {
        match self {
            AgentKind::Implementer => crate::domain::role::Role::Implementer,
            AgentKind::Reviewer => crate::domain::role::Role::Reviewer,
            AgentKind::Coordinator => crate::domain::role::Role::Coordinator,
            AgentKind::Researcher => crate::domain::role::Role::Researcher,
            AgentKind::Integrator => crate::domain::role::Role::Integrator,
            AgentKind::Chat => crate::domain::role::Role::Coordinator,
            AgentKind::Decomposer => crate::domain::role::Role::Decomposer,
        }
    }

    /// Returns true if this agent type operates in the "thinking plane" (no worktree needed).
    pub fn is_thinking_plane(&self) -> bool {
        matches!(
            self,
            AgentKind::Coordinator
                | AgentKind::Researcher
                | AgentKind::Integrator
                | AgentKind::Reviewer
                | AgentKind::Chat
                | AgentKind::Decomposer
        )
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentKind::Implementer => write!(f, "implementer"),
            AgentKind::Reviewer => write!(f, "reviewer"),
            AgentKind::Coordinator => write!(f, "coordinator"),
            AgentKind::Researcher => write!(f, "researcher"),
            AgentKind::Integrator => write!(f, "integrator"),
            AgentKind::Chat => write!(f, "chat"),
            AgentKind::Decomposer => write!(f, "decomposer"),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    const ALL_AGENT_TYPES: [AgentKind; 6] = [
        AgentKind::Implementer,
        AgentKind::Reviewer,
        AgentKind::Coordinator,
        AgentKind::Researcher,
        AgentKind::Integrator,
        AgentKind::Decomposer,
    ];

    #[test]
    fn test_agent_type_display() {
        assert_eq!(AgentKind::Implementer.to_string(), "implementer");
        assert_eq!(AgentKind::Reviewer.to_string(), "reviewer");
        assert_eq!(AgentKind::Coordinator.to_string(), "coordinator");
        assert_eq!(AgentKind::Researcher.to_string(), "researcher");
        assert_eq!(AgentKind::Integrator.to_string(), "integrator");
        assert_eq!(AgentKind::Decomposer.to_string(), "decomposer");
    }

    #[test]
    fn test_agent_type_serde_roundtrip() {
        for at in ALL_AGENT_TYPES {
            let json = serde_json::to_string(&at).unwrap();
            let deserialized: AgentKind = serde_json::from_str(&json).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_display_matches_serde() {
        for at in ALL_AGENT_TYPES {
            let display = at.to_string();
            let quoted = format!("\"{display}\"");
            let deserialized: AgentKind = serde_json::from_str(&quoted).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_default_role() {
        use crate::domain::role::Role;
        assert_eq!(AgentKind::Implementer.default_role(), Role::Implementer);
        assert_eq!(AgentKind::Reviewer.default_role(), Role::Reviewer);
        assert_eq!(AgentKind::Coordinator.default_role(), Role::Coordinator);
        assert_eq!(AgentKind::Researcher.default_role(), Role::Researcher);
        assert_eq!(AgentKind::Integrator.default_role(), Role::Integrator);
        assert_eq!(AgentKind::Decomposer.default_role(), Role::Decomposer);
    }

    #[test]
    fn test_agent_type_is_thinking_plane() {
        assert!(!AgentKind::Implementer.is_thinking_plane());
        assert!(AgentKind::Reviewer.is_thinking_plane());
        assert!(AgentKind::Coordinator.is_thinking_plane());
        assert!(AgentKind::Researcher.is_thinking_plane());
        assert!(AgentKind::Integrator.is_thinking_plane());
        assert!(AgentKind::Decomposer.is_thinking_plane());
    }
}
