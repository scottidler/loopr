use std::fmt;

use derive::Record;
use serde::{Deserialize, Serialize};
use taskstore_traits::{IndexValue, Record as RecordTrait};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FakeStatus {
    Draft,
    Ready,
    Done,
}

impl fmt::Display for FakeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FakeStatus::Draft => "draft",
            FakeStatus::Ready => "ready",
            FakeStatus::Done => "done",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FakeTier {
    Tier1,
    Tier2,
}

impl fmt::Display for FakeTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FakeTier::Tier1 => "tier1",
            FakeTier::Tier2 => "tier2",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
struct Plan {
    id: String,
    updated_at: i64,
    #[record(indexed)]
    status: FakeStatus,
    #[record(indexed)]
    tier: FakeTier,
    #[allow(dead_code)]
    goal: String,
}

#[test]
fn plan_default_collection_is_struct_ident_lowercased_plus_s() {
    assert_eq!(Plan::collection_name(), "plans");
}

#[test]
fn plan_id_returns_inner_str() {
    let p = Plan {
        id: "plan-42".to_string(),
        updated_at: 1_700_000_000_000,
        status: FakeStatus::Draft,
        tier: FakeTier::Tier1,
        goal: "ship it".to_string(),
    };
    assert_eq!(p.id(), "plan-42");
}

#[test]
fn plan_updated_at_returns_i64() {
    let p = Plan {
        id: "plan-42".to_string(),
        updated_at: 1_700_000_000_000,
        status: FakeStatus::Draft,
        tier: FakeTier::Tier1,
        goal: "ship it".to_string(),
    };
    assert_eq!(p.updated_at(), 1_700_000_000_000);
}

#[test]
fn plan_indexed_fields_contains_two_entries() {
    let p = Plan {
        id: "plan-42".to_string(),
        updated_at: 1,
        status: FakeStatus::Ready,
        tier: FakeTier::Tier2,
        goal: String::new(),
    };
    let m = p.indexed_fields();
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("status"), Some(&IndexValue::String("ready".to_string())));
    assert_eq!(m.get("tier"), Some(&IndexValue::String("tier2".to_string())));
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[record(collection = "plans-v2")]
struct PlanOverride {
    id: String,
    updated_at: i64,
}

#[test]
fn collection_override_wins_over_default() {
    assert_eq!(PlanOverride::collection_name(), "plans-v2");
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
struct EmptyIndex {
    id: String,
    updated_at: i64,
    #[allow(dead_code)]
    goal: String,
}

#[test]
fn zero_indexed_fields_produces_empty_map() {
    let e = EmptyIndex {
        id: "e-1".to_string(),
        updated_at: 0,
        goal: String::new(),
    };
    assert!(e.indexed_fields().is_empty());
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PlanId(String);

impl AsRef<str> for PlanId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
struct TypedIdPlan {
    id: PlanId,
    updated_at: i64,
}

#[test]
fn typed_id_newtype_works_via_as_ref() {
    let p = TypedIdPlan {
        id: PlanId("plan-99".to_string()),
        updated_at: 0,
    };
    assert_eq!(p.id(), "plan-99");
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
struct OptionalIndexPlan {
    id: String,
    updated_at: i64,
    #[record(indexed)]
    parent_id: Option<String>,
}

#[test]
fn option_none_omits_map_entry() {
    let p = OptionalIndexPlan {
        id: "x".to_string(),
        updated_at: 0,
        parent_id: None,
    };
    let m = p.indexed_fields();
    assert!(m.is_empty());
}

#[test]
fn option_some_inserts_inner_value() {
    let p = OptionalIndexPlan {
        id: "x".to_string(),
        updated_at: 0,
        parent_id: Some("parent-7".to_string()),
    };
    let m = p.indexed_fields();
    assert_eq!(m.len(), 1);
    assert_eq!(m.get("parent_id"), Some(&IndexValue::String("parent-7".to_string())),);
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
struct KeyOverridePlan {
    id: String,
    updated_at: i64,
    #[record(indexed(key = "parentId"))]
    parent_id: Option<String>,
}

#[test]
fn key_override_replaces_map_key_not_field_name() {
    let p = KeyOverridePlan {
        id: "x".to_string(),
        updated_at: 0,
        parent_id: Some("parent-7".to_string()),
    };
    let m = p.indexed_fields();
    assert_eq!(m.len(), 1);
    assert!(m.contains_key("parentId"));
    assert!(!m.contains_key("parent_id"));
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
struct BareStringIndex {
    id: String,
    updated_at: i64,
    #[record(indexed)]
    owner: String,
}

#[test]
fn string_indexed_field_uses_to_string() {
    let r = BareStringIndex {
        id: "r-1".to_string(),
        updated_at: 0,
        owner: "alice".to_string(),
    };
    assert_eq!(
        r.indexed_fields().get("owner"),
        Some(&IndexValue::String("alice".to_string())),
    );
}
