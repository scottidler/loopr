//! Storage trait definitions and filter types.

use crate::error::Result;
use serde::{de::DeserializeOwned, Serialize};

/// Filter operations for querying records.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterOp {
    /// Field equals value
    Eq,
    /// Field does not equal value
    Ne,
    /// Field contains value (string/array)
    Contains,
}

/// A filter for querying records.
#[derive(Debug, Clone)]
pub struct Filter {
    /// Field name to filter on
    pub field: String,
    /// Filter operation
    pub op: FilterOp,
    /// Value to compare against
    pub value: serde_json::Value,
}

impl Filter {
    /// Create an equality filter.
    pub fn eq(field: impl Into<String>, value: impl Serialize) -> Self {
        Self {
            field: field.into(),
            op: FilterOp::Eq,
            value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }

    /// Create a not-equal filter.
    pub fn ne(field: impl Into<String>, value: impl Serialize) -> Self {
        Self {
            field: field.into(),
            op: FilterOp::Ne,
            value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }

    /// Create a contains filter.
    pub fn contains(field: impl Into<String>, value: impl Serialize) -> Self {
        Self {
            field: field.into(),
            op: FilterOp::Contains,
            value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }

    /// Check if a record matches this filter.
    pub fn matches(&self, record: &serde_json::Value) -> bool {
        let field_value = record.get(&self.field);

        match &self.op {
            FilterOp::Eq => match field_value {
                Some(v) => *v == self.value,
                None => self.value.is_null(),
            },
            FilterOp::Ne => match field_value {
                Some(v) => *v != self.value,
                None => !self.value.is_null(),
            },
            FilterOp::Contains => match field_value {
                Some(serde_json::Value::String(s)) => {
                    if let serde_json::Value::String(needle) = &self.value {
                        s.contains(needle.as_str())
                    } else {
                        false
                    }
                }
                Some(serde_json::Value::Array(arr)) => arr.contains(&self.value),
                _ => false,
            },
        }
    }
}

/// Trait for records that have an ID field.
pub trait HasId {
    /// Get the record's unique identifier.
    fn id(&self) -> &str;
}

/// Storage trait for CRUD operations on records.
pub trait Storage: Send + Sync {
    /// Create a new record.
    fn create<T: Serialize + DeserializeOwned + HasId>(&self, collection: &str, record: &T) -> Result<()>;

    /// Get a record by ID.
    fn get<T: DeserializeOwned>(&self, collection: &str, id: &str) -> Result<Option<T>>;

    /// Update an existing record.
    fn update<T: Serialize + DeserializeOwned + HasId>(&self, collection: &str, id: &str, record: &T) -> Result<()>;

    /// Delete a record by ID.
    fn delete(&self, collection: &str, id: &str) -> Result<()>;

    /// Query records with filters.
    fn query<T: DeserializeOwned>(&self, collection: &str, filters: &[Filter]) -> Result<Vec<T>>;

    /// List all records in a collection.
    fn list<T: DeserializeOwned>(&self, collection: &str) -> Result<Vec<T>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_filter_eq_matches() {
        let filter = Filter::eq("status", "running");
        let record = json!({"id": "1", "status": "running"});
        assert!(filter.matches(&record));
    }

    #[test]
    fn test_filter_eq_no_match() {
        let filter = Filter::eq("status", "running");
        let record = json!({"id": "1", "status": "pending"});
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_eq_null() {
        let filter = Filter::eq("field", serde_json::Value::Null);
        let record = json!({"id": "1"});
        assert!(filter.matches(&record));
    }

    #[test]
    fn test_filter_ne_matches() {
        let filter = Filter::ne("status", "running");
        let record = json!({"id": "1", "status": "pending"});
        assert!(filter.matches(&record));
    }

    #[test]
    fn test_filter_ne_no_match() {
        let filter = Filter::ne("status", "running");
        let record = json!({"id": "1", "status": "running"});
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_contains_string() {
        let filter = Filter::contains("name", "test");
        let record = json!({"id": "1", "name": "my_test_file"});
        assert!(filter.matches(&record));
    }

    #[test]
    fn test_filter_contains_string_no_match() {
        let filter = Filter::contains("name", "foo");
        let record = json!({"id": "1", "name": "bar"});
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_contains_array() {
        let filter = Filter::contains("tags", "important");
        let record = json!({"id": "1", "tags": ["important", "urgent"]});
        assert!(filter.matches(&record));
    }

    #[test]
    fn test_filter_contains_array_no_match() {
        let filter = Filter::contains("tags", "missing");
        let record = json!({"id": "1", "tags": ["important", "urgent"]});
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_op_enum_equality() {
        assert_eq!(FilterOp::Eq, FilterOp::Eq);
        assert_ne!(FilterOp::Eq, FilterOp::Ne);
        assert_ne!(FilterOp::Ne, FilterOp::Contains);
    }
}
