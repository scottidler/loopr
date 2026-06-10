use super::*;

#[test]
fn kind_from_prefix_routes_every_current_record() {
    assert_eq!(kind_from_prefix("pl-abcde").unwrap(), RecordKind::Plan);
    assert_eq!(kind_from_prefix("wk-abcde").unwrap(), RecordKind::Work);
    assert_eq!(kind_from_prefix("bd-abcde").unwrap(), RecordKind::Bundle);
    assert_eq!(kind_from_prefix("tk-abcde").unwrap(), RecordKind::Tick);
}

#[test]
fn kind_from_prefix_rejects_unknown_prefix() {
    match kind_from_prefix("xx-abcde") {
        Err(LooprError::UnknownIdPrefix { id }) => assert_eq!(id, "xx-abcde"),
        other => panic!("expected UnknownIdPrefix, got {other:?}"),
    }
}

#[test]
fn kind_from_prefix_rejects_id_with_no_dash() {
    match kind_from_prefix("plabcde") {
        Err(LooprError::UnknownIdPrefix { id }) => assert_eq!(id, "plabcde"),
        other => panic!("expected UnknownIdPrefix, got {other:?}"),
    }
}

#[test]
fn kind_from_prefix_rejects_empty_string() {
    match kind_from_prefix("") {
        Err(LooprError::UnknownIdPrefix { id }) => assert!(id.is_empty()),
        other => panic!("expected UnknownIdPrefix, got {other:?}"),
    }
}

#[test]
fn kind_from_prefix_accepts_truncated_id_with_dash() {
    // An id of just `pl-` with nothing after is structurally weird but
    // the prefix check accepts it; the daemon will then return
    // NotFound. This keeps `show` prefix-routing cheap and honest about
    // what it validates (prefix only, not the id body).
    assert_eq!(kind_from_prefix("pl-").unwrap(), RecordKind::Plan);
}

#[test]
fn validate_kind_match_accepts_matching_kind() {
    let result = RecordResult::Plan(domain::Plan::new("goal".to_string()));
    assert!(validate_kind_match(&result, RecordKind::Plan).is_ok());
}

#[test]
fn validate_kind_match_rejects_mismatched_kind() {
    // Prefix said Work but the daemon returned a Plan: a protocol mismatch.
    let result = RecordResult::Plan(domain::Plan::new("goal".to_string()));
    match validate_kind_match(&result, RecordKind::Work) {
        Err(LooprError::ClientIo(msg)) => assert!(msg.contains("protocol mismatch"), "got: {msg}"),
        other => panic!("expected ClientIo protocol mismatch, got {other:?}"),
    }
}
