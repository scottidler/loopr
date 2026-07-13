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

/// A healthy random generator varies every body position across a sample. A
/// stuck RNG or a constant/degenerate generator collapses one or more
/// positions to a single char; this catches that.
///
/// We assert per-position variation, NOT global uniqueness of the sample.
/// Uniqueness is a birthday-paradox property: the body is 5 base36 chars
/// (36^5 ≈ 60.4M space), so 1000 draws collide ~0.8% of the time. Asserting
/// "all N distinct" is therefore a statistically-false claim about a random
/// generator (it flaked ~1.3%/run before this change), not a real invariant.
/// A false failure of THIS test needs all `SAMPLES` ids to share a char at
/// some position (~36^(1-SAMPLES)), i.e. never. Collision *handling* is
/// covered where it matters — at the store seam (`store::{works,plans,
/// bundles,ticks}` return `AlreadyExists`; the decomposer's
/// `persist_works_with_remint` re-mints on collision).
#[test]
fn generate_id_body_positions_are_not_stuck() {
    const SAMPLES: usize = 256;
    const BODY_LEN: usize = 5;
    let bodies: Vec<Vec<char>> = (0..SAMPLES)
        .map(|_| generate_id("wk").chars().skip(3).collect())
        .collect();
    for pos in 0..BODY_LEN {
        let distinct: HashSet<char> = bodies.iter().map(|b| b[pos]).collect();
        assert!(
            distinct.len() > 1,
            "body position {pos} produced only {distinct:?} across {SAMPLES} ids (stuck RNG?)"
        );
    }
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

/// `WorkId::new` twin of `generate_id_body_positions_are_not_stuck`: assert
/// per-position variation, not global uniqueness (see that test for why
/// uniqueness is a birthday-paradox property, not an invariant).
#[test]
fn work_id_body_positions_are_not_stuck() {
    const SAMPLES: usize = 256;
    const BODY_LEN: usize = 5;
    let bodies: Vec<Vec<char>> = (0..SAMPLES)
        .map(|_| WorkId::new().as_ref().chars().skip(3).collect())
        .collect();
    for pos in 0..BODY_LEN {
        let distinct: HashSet<char> = bodies.iter().map(|b| b[pos]).collect();
        assert!(
            distinct.len() > 1,
            "WorkId body position {pos} produced only {distinct:?} across {SAMPLES} ids (stuck RNG?)"
        );
    }
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
