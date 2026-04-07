use serde::{Deserialize, Serialize};

/// Acceptance criteria for any hierarchy level (Plan, Spec, Phase, Work).
///
/// Wraps a `Vec<String>` so all four levels share the same type and the
/// serde/display logic lives in one place.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AcceptanceCriteria(pub Vec<String>);

impl AcceptanceCriteria {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acceptance_criteria_default_empty() {
        let ac = AcceptanceCriteria::default();
        assert!(ac.is_empty());
        assert_eq!(ac.len(), 0);
    }

    #[test]
    fn test_acceptance_criteria_with_items() {
        let ac = AcceptanceCriteria(vec!["cargo test passes".to_string(), "clippy clean".to_string()]);
        assert!(!ac.is_empty());
        assert_eq!(ac.len(), 2);
    }

    #[test]
    fn test_acceptance_criteria_serde_roundtrip() {
        let ac = AcceptanceCriteria(vec!["item one".to_string(), "item two".to_string()]);
        let json = serde_json::to_string(&ac).unwrap();
        let restored: AcceptanceCriteria = serde_json::from_str(&json).unwrap();
        assert_eq!(ac, restored);
    }

    #[test]
    fn test_acceptance_criteria_serde_format() {
        let ac = AcceptanceCriteria(vec!["a".to_string(), "b".to_string()]);
        let json = serde_json::to_string(&ac).unwrap();
        // Serializes as a JSON array (the inner Vec<String>)
        assert_eq!(json, r#"["a","b"]"#);
    }

    #[test]
    fn test_acceptance_criteria_deserializes_empty_array() {
        let ac: AcceptanceCriteria = serde_json::from_str("[]").unwrap();
        assert!(ac.is_empty());
    }
}
