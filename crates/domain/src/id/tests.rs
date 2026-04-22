#![allow(clippy::unwrap_used)]

use super::*;
use std::collections::HashSet;
use std::str::FromStr;

#[test]
fn generate_id_format() {
    let id = generate_id("pl");
    assert_eq!(id.len(), 8, "id must be 8 chars total: {id}");
    assert!(id.starts_with("pl-"), "id must start with prefix: {id}");
}

#[test]
fn generate_id_base36_chars() {
    let id = generate_id("xx");
    let code = &id[3..];
    assert!(
        code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        "body must be base36 lowercase: {code}"
    );
}

#[test]
fn generate_id_uniqueness_1000() {
    let ids: HashSet<String> = (0..1000).map(|_| generate_id("wk")).collect();
    assert_eq!(ids.len(), 1000, "expected 1000 distinct ids");
}

#[test]
fn now_millis_reasonable() {
    let ts = now_millis();
    let year_2020_ms: i64 = 1_577_836_800_000;
    let year_2100_ms: i64 = 4_102_444_800_000;
    assert!(ts > year_2020_ms, "timestamp should be after 2020: {ts}");
    assert!(ts < year_2100_ms, "timestamp should be before 2100: {ts}");
}

#[test]
fn plan_id_new_has_prefix() {
    let id = PlanId::new();
    assert!(
        id.as_ref().starts_with("pl-"),
        "PlanId::new should produce pl-prefixed id: {id}"
    );
    assert_eq!(PlanId::prefix(), "pl");
}

#[test]
fn plan_id_serde_roundtrip() {
    let id = PlanId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: PlanId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn plan_id_serde_wire_form_is_bare_string() {
    // With #[serde(transparent)], the wire form is a bare JSON string, not a
    // one-element array. Without the attribute, serde would emit
    // ["pl-xxxxx"] for a tuple-struct newtype. This test pins the attribute.
    let id = PlanId::new();
    let json = serde_json::to_string(&id).unwrap();
    assert!(
        json.starts_with('"') && json.ends_with('"'),
        "expected bare JSON string, got: {json}"
    );
    assert!(!json.starts_with('['), "wire form must not be a JSON array: {json}");
}

#[test]
fn plan_id_from_str_roundtrip() {
    let original = "pl-k7m2p";
    let id = PlanId::from_str(original).unwrap();
    assert_eq!(id.as_ref(), original);
}

#[test]
fn plan_id_display_matches_as_ref() {
    let id = PlanId::new();
    assert_eq!(id.to_string(), id.as_ref());
}

#[test]
fn work_id_new_has_prefix() {
    let id = WorkId::new();
    assert!(
        id.as_ref().starts_with("wk-"),
        "WorkId::new should produce wk-prefixed id: {id}"
    );
    assert_eq!(WorkId::prefix(), "wk");
}

#[test]
fn work_id_serde_roundtrip() {
    let id = WorkId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: WorkId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn work_id_serde_wire_form_is_bare_string() {
    let id = WorkId::new();
    let json = serde_json::to_string(&id).unwrap();
    assert!(
        json.starts_with('"') && json.ends_with('"'),
        "expected bare JSON string, got: {json}"
    );
    assert!(!json.starts_with('['), "wire form must not be a JSON array: {json}");
}

#[test]
fn work_id_from_str_roundtrip() {
    let original = "wk-k7m2p";
    let id = WorkId::from_str(original).unwrap();
    assert_eq!(id.as_ref(), original);
}

#[test]
fn work_id_display_matches_as_ref() {
    let id = WorkId::new();
    assert_eq!(id.to_string(), id.as_ref());
}

#[test]
fn work_id_uniqueness_1000() {
    let ids: HashSet<String> = (0..1000).map(|_| WorkId::new().as_ref().to_string()).collect();
    assert_eq!(ids.len(), 1000, "expected 1000 distinct WorkIds");
}

#[test]
fn bundle_id_new_has_prefix() {
    let id = BundleId::new();
    assert!(
        id.as_ref().starts_with("bd-"),
        "BundleId::new should produce bd-prefixed id: {id}"
    );
    assert_eq!(BundleId::prefix(), "bd");
}

#[test]
fn bundle_id_serde_roundtrip() {
    let id = BundleId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: BundleId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn bundle_id_serde_wire_form_is_bare_string() {
    let id = BundleId::new();
    let json = serde_json::to_string(&id).unwrap();
    assert!(
        json.starts_with('"') && json.ends_with('"'),
        "expected bare JSON string, got: {json}"
    );
    assert!(!json.starts_with('['), "wire form must not be a JSON array: {json}");
}

#[test]
fn bundle_id_from_str_roundtrip() {
    let original = "bd-k7m2p";
    let id = BundleId::from_str(original).unwrap();
    assert_eq!(id.as_ref(), original);
}

#[test]
fn bundle_id_display_matches_as_ref() {
    let id = BundleId::new();
    assert_eq!(id.to_string(), id.as_ref());
}

#[test]
fn tick_id_new_has_prefix() {
    let id = TickId::new();
    assert!(
        id.as_ref().starts_with("tk-"),
        "TickId::new should produce tk-prefixed id: {id}"
    );
    assert_eq!(TickId::prefix(), "tk");
}

#[test]
fn tick_id_serde_roundtrip() {
    let id = TickId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: TickId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn tick_id_serde_wire_form_is_bare_string() {
    let id = TickId::new();
    let json = serde_json::to_string(&id).unwrap();
    assert!(
        json.starts_with('"') && json.ends_with('"'),
        "expected bare JSON string, got: {json}"
    );
    assert!(!json.starts_with('['), "wire form must not be a JSON array: {json}");
}

#[test]
fn tick_id_from_str_roundtrip() {
    let original = "tk-k7m2p";
    let id = TickId::from_str(original).unwrap();
    assert_eq!(id.as_ref(), original);
}

#[test]
fn tick_id_display_matches_as_ref() {
    let id = TickId::new();
    assert_eq!(id.to_string(), id.as_ref());
}
