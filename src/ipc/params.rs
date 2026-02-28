//! Typed IPC parameter structs for compile-time safety between executor and handler.
//!
//! These structs are shared by the executor (serialization) and handler (deserialization),
//! ensuring type agreement at compile time. Introduced in MVP7 for the 3 highest-risk methods.

use serde::{Deserialize, Serialize};

use crate::domain::learning::LearningScope;
use crate::domain::role::Role;

/// Params for `bundle.create` IPC method.
#[derive(Debug, Serialize, Deserialize)]
pub struct BundleCreateParams {
    pub work_item_id: String,
    pub branch_name: String,
    pub claims: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub base_tick_id: Option<String>,
    #[serde(default)]
    pub touched_paths: Vec<String>,
}

/// Params for `learning.create` IPC method.
#[derive(Debug, Serialize, Deserialize)]
pub struct LearningCreateParams {
    pub content: String,
    pub scope: LearningScope,
    pub source_id: String,
    #[serde(default)]
    pub applicable_roles: Option<Vec<Role>>,
    #[serde(default)]
    pub resource_tags: Option<Vec<String>>,
}

/// Params for `worktree.refresh` IPC method.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorktreeRefreshParams {
    pub work_item_id: String,
    #[serde(default = "default_base_ref")]
    pub new_base_ref: String,
}

fn default_base_ref() -> String {
    "HEAD".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_create_params_roundtrip() {
        let params = BundleCreateParams {
            work_item_id: "wi-1".to_string(),
            branch_name: "feature/test".to_string(),
            claims: vec!["claim1".to_string(), "claim2".to_string()],
            description: Some("A test bundle".to_string()),
            base_tick_id: Some("tick-1".to_string()),
            touched_paths: vec!["src/main.rs".to_string()],
        };
        let json = serde_json::to_value(&params).unwrap();
        let restored: BundleCreateParams = serde_json::from_value(json).unwrap();
        assert_eq!(restored.work_item_id, "wi-1");
        assert_eq!(restored.claims, vec!["claim1", "claim2"]);
        assert_eq!(restored.description, Some("A test bundle".to_string()));
    }

    #[test]
    fn test_bundle_create_params_defaults() {
        let json = serde_json::json!({
            "work_item_id": "wi-1",
            "branch_name": "feature/x",
            "claims": ["c1"]
        });
        let params: BundleCreateParams = serde_json::from_value(json).unwrap();
        assert!(params.description.is_none());
        assert!(params.base_tick_id.is_none());
        assert!(params.touched_paths.is_empty());
    }

    #[test]
    fn test_learning_create_params_roundtrip() {
        let params = LearningCreateParams {
            content: "insight".to_string(),
            scope: LearningScope::Phase,
            source_id: "wi-1".to_string(),
            applicable_roles: Some(vec![Role::Implementer, Role::Reviewer]),
            resource_tags: Some(vec!["src/main.rs".to_string()]),
        };
        let json = serde_json::to_value(&params).unwrap();
        let restored: LearningCreateParams = serde_json::from_value(json).unwrap();
        assert_eq!(restored.content, "insight");
        assert_eq!(restored.scope, LearningScope::Phase);
        assert_eq!(restored.applicable_roles.unwrap().len(), 2);
    }

    #[test]
    fn test_learning_create_params_defaults() {
        let json = serde_json::json!({
            "content": "test",
            "scope": "phase",
            "source_id": "wi-1"
        });
        let params: LearningCreateParams = serde_json::from_value(json).unwrap();
        assert!(params.applicable_roles.is_none());
        assert!(params.resource_tags.is_none());
    }

    #[test]
    fn test_worktree_refresh_params_roundtrip() {
        let params = WorktreeRefreshParams {
            work_item_id: "wi-1".to_string(),
            new_base_ref: "abc123".to_string(),
        };
        let json = serde_json::to_value(&params).unwrap();
        let restored: WorktreeRefreshParams = serde_json::from_value(json).unwrap();
        assert_eq!(restored.work_item_id, "wi-1");
        assert_eq!(restored.new_base_ref, "abc123");
    }

    #[test]
    fn test_worktree_refresh_params_default_base_ref() {
        let json = serde_json::json!({
            "work_item_id": "wi-1"
        });
        let params: WorktreeRefreshParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.new_base_ref, "HEAD");
    }

    #[test]
    fn test_hierarchy_status_pascal_case_alias() {
        use crate::domain::plan::HierarchyStatus;
        let status: HierarchyStatus = serde_json::from_str("\"Draft\"").unwrap();
        assert_eq!(status, HierarchyStatus::Draft);
        let status2: HierarchyStatus = serde_json::from_str("\"Active\"").unwrap();
        assert_eq!(status2, HierarchyStatus::Active);
        let status3: HierarchyStatus = serde_json::from_str("\"Complete\"").unwrap();
        assert_eq!(status3, HierarchyStatus::Complete);
        let status4: HierarchyStatus = serde_json::from_str("\"Abandoned\"").unwrap();
        assert_eq!(status4, HierarchyStatus::Abandoned);
    }

    #[test]
    fn test_hierarchy_status_lowercase_canonical() {
        use crate::domain::plan::HierarchyStatus;
        let status: HierarchyStatus = serde_json::from_str("\"draft\"").unwrap();
        assert_eq!(status, HierarchyStatus::Draft);
    }
}
