use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Acceptance criteria for any hierarchy level (Plan, Spec, Phase, Work).
///
/// Wraps a `Vec<String>` so all four levels share the same type and the
/// serde/display logic lives in one place.
///
/// Deserializes from both:
/// - A JSON array of strings: `["pass tests", "clippy clean"]` (current format)
/// - A plain JSON string: `"pass tests"` (legacy single-string format for Plan)
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct AcceptanceCriteria(pub Vec<String>);

impl From<String> for AcceptanceCriteria {
    fn from(s: String) -> Self {
        if s.is_empty() { Self::default() } else { Self(vec![s]) }
    }
}

impl From<&str> for AcceptanceCriteria {
    fn from(s: &str) -> Self {
        Self::from(s.to_string())
    }
}

impl FromIterator<String> for AcceptanceCriteria {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'de> Deserialize<'de> for AcceptanceCriteria {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AcVisitor)
    }
}

struct AcVisitor;

impl<'de> Visitor<'de> for AcVisitor {
    type Value = AcceptanceCriteria;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string or array of strings")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<AcceptanceCriteria, E> {
        Ok(AcceptanceCriteria::from(v))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<AcceptanceCriteria, E> {
        Ok(AcceptanceCriteria::from(v))
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<AcceptanceCriteria, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<String>()? {
            items.push(item);
        }
        Ok(AcceptanceCriteria(items))
    }
}

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
