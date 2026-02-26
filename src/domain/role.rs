use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Coordinator,
    Integrator,
    Implementer,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Coordinator => write!(f, "coordinator"),
            Role::Integrator => write!(f, "integrator"),
            Role::Implementer => write!(f, "implementer"),
        }
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "coordinator" => Ok(Role::Coordinator),
            "integrator" => Ok(Role::Integrator),
            "implementer" => Ok(Role::Implementer),
            _ => Err(format!(
                "unknown role: '{s}' (expected: Coordinator, Integrator, Implementer)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_display() {
        assert_eq!(Role::Coordinator.to_string(), "coordinator");
        assert_eq!(Role::Integrator.to_string(), "integrator");
        assert_eq!(Role::Implementer.to_string(), "implementer");
    }

    #[test]
    fn test_role_equality() {
        assert_eq!(Role::Coordinator, Role::Coordinator);
        assert_ne!(Role::Coordinator, Role::Integrator);
        assert_ne!(Role::Integrator, Role::Implementer);
    }

    #[test]
    fn test_role_copy() {
        let role = Role::Coordinator;
        let copied = role;
        assert_eq!(role, copied);
    }

    #[test]
    fn test_role_serde_roundtrip() {
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            let json = serde_json::to_string(&role).unwrap();
            let deserialized: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(role, deserialized);
        }
    }

    #[test]
    fn test_role_serde_format() {
        assert_eq!(serde_json::to_string(&Role::Coordinator).unwrap(), "\"coordinator\"");
        assert_eq!(serde_json::to_string(&Role::Integrator).unwrap(), "\"integrator\"");
        assert_eq!(serde_json::to_string(&Role::Implementer).unwrap(), "\"implementer\"");
    }

    #[test]
    fn test_role_display_matches_serde() {
        // Regression: Display must produce values that serde can deserialize.
        // CLI dispatch uses to_string() but handlers use serde_json::from_value().
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            let display = role.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: Role = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(role, deserialized);
        }
    }
}
