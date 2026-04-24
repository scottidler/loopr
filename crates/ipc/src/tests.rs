use serde_json::json;

use domain::Plan;

use crate::envelope::{DaemonEvent, DaemonRequest, DaemonResponse};
use crate::error::RpcError;
use crate::frame::{ParseError, decode_line, decode_request_line, encode_line};
use crate::method::{HandshakeParams, HandshakeResult, Method, PlanCreateParams, PlanCreateResult, StatusResult};
use crate::records::{
    BundleSummary, PlanSummary, RecordGetParams, RecordKind, RecordListParams, RecordResult, RecordsResult,
};
use crate::{IpcMessage, MAX_LINE_BYTES, PROTOCOL_VERSION};

// --- Phase 2: envelope construction invariants ---

#[test]
fn response_ok_invariants() {
    let r = DaemonResponse::ok(1, json!("hello"));
    assert!(r.result.is_some());
    assert!(r.error.is_none());
    assert!(!r.is_error());
}

#[test]
fn response_err_invariants() {
    let r = DaemonResponse::err(2, RpcError::Internal("boom".into()));
    assert!(r.result.is_none());
    assert!(r.error.is_some());
    assert!(r.is_error());
}

// --- Phase 2: RpcError round-trip through serde_json (all 12 named codes) ---

fn rpc_roundtrip(err: RpcError) -> RpcError {
    let s = serde_json::to_string(&err).unwrap();
    serde_json::from_str(&s).unwrap()
}

#[test]
fn rpc_error_roundtrip_parse_error() {
    let e = RpcError::ParseError("bad json".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_invalid_request() {
    let e = RpcError::InvalidRequest("malformed".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_method_not_found() {
    let e = RpcError::MethodNotFound("foo.bar".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_invalid_params() {
    let e = RpcError::InvalidParams("missing field".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_internal() {
    let e = RpcError::Internal("oops".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_transition_rejected() {
    let e = RpcError::TransitionRejected("bad state".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_not_found() {
    let e = RpcError::NotFound("plan-1".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_stale_bundle() {
    let e = RpcError::StaleBundle("bundle-x".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_validation_required() {
    let e = RpcError::ValidationRequired("work-3".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_pool_exhausted() {
    let e = RpcError::PoolExhausted("all slots taken".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_precondition_failed() {
    let e = RpcError::PreconditionFailed("deps unmet".into());
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_roundtrip_protocol_version_mismatch() {
    let e = RpcError::protocol_version_mismatch(1, 2);
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_unknown_forward_compat() {
    let e = RpcError::Unknown {
        code: -32042,
        message: "future error".into(),
    };
    assert_eq!(rpc_roundtrip(e.clone()), e);
}

#[test]
fn rpc_error_code_constants() {
    assert_eq!(RpcError::ParseError("".into()).code(), RpcError::CODE_PARSE_ERROR);
    assert_eq!(
        RpcError::InvalidRequest("".into()).code(),
        RpcError::CODE_INVALID_REQUEST
    );
    assert_eq!(
        RpcError::MethodNotFound("".into()).code(),
        RpcError::CODE_METHOD_NOT_FOUND
    );
    assert_eq!(RpcError::InvalidParams("".into()).code(), RpcError::CODE_INVALID_PARAMS);
    assert_eq!(RpcError::Internal("".into()).code(), RpcError::CODE_INTERNAL);
    assert_eq!(
        RpcError::TransitionRejected("".into()).code(),
        RpcError::CODE_TRANSITION_REJECTED
    );
    assert_eq!(RpcError::NotFound("".into()).code(), RpcError::CODE_NOT_FOUND);
    assert_eq!(RpcError::StaleBundle("".into()).code(), RpcError::CODE_STALE_BUNDLE);
    assert_eq!(
        RpcError::ValidationRequired("".into()).code(),
        RpcError::CODE_VALIDATION_REQUIRED
    );
    assert_eq!(RpcError::PoolExhausted("".into()).code(), RpcError::CODE_POOL_EXHAUSTED);
    assert_eq!(
        RpcError::PreconditionFailed("".into()).code(),
        RpcError::CODE_PRECONDITION_FAILED
    );
    assert_eq!(
        RpcError::ProtocolVersionMismatch("".into()).code(),
        RpcError::CODE_PROTOCOL_VERSION_MISMATCH
    );
}

// --- Phase 3: frame encode/decode ---

#[test]
fn encode_line_ends_with_newline() {
    let req = DaemonRequest {
        id: 1,
        method: "system.status".into(),
        params: json!(null),
    };
    let bytes = encode_line(&req);
    assert_eq!(bytes.last(), Some(&b'\n'));
}

#[test]
fn encode_line_byte_stability() {
    let req = DaemonRequest {
        id: 1,
        method: "system.status".into(),
        params: json!(null),
    };
    let a = encode_line(&req);
    let b = encode_line(&req);
    assert_eq!(a, b);
}

#[test]
fn decode_line_response_ok() {
    let resp = DaemonResponse::ok(1, json!({"status": "ok"}));
    let bytes = encode_line(&resp);
    match decode_line(&bytes).unwrap() {
        IpcMessage::Response(r) => {
            assert_eq!(r.id, 1);
            assert!(!r.is_error());
        }
        IpcMessage::Event(_) => panic!("expected Response"),
    }
}

#[test]
fn decode_line_response_err() {
    let resp = DaemonResponse::err(2, RpcError::Internal("fail".into()));
    let bytes = encode_line(&resp);
    match decode_line(&bytes).unwrap() {
        IpcMessage::Response(r) => {
            assert_eq!(r.id, 2);
            assert!(r.is_error());
        }
        IpcMessage::Event(_) => panic!("expected Response"),
    }
}

#[test]
fn decode_line_event() {
    let event = DaemonEvent {
        event: "tick.published".into(),
        data: json!({"tick-id": "t1"}),
    };
    let bytes = encode_line(&event);
    match decode_line(&bytes).unwrap() {
        IpcMessage::Event(e) => assert_eq!(e.event, "tick.published"),
        IpcMessage::Response(_) => panic!("expected Event"),
    }
}

#[test]
fn decode_line_oversize_returns_line_too_long() {
    let big = vec![b'x'; MAX_LINE_BYTES + 1];
    match decode_line(&big) {
        Err(ParseError::LineTooLong { size }) => assert_eq!(size, MAX_LINE_BYTES + 1),
        other => panic!("expected LineTooLong, got {other:?}"),
    }
}

#[test]
fn decode_line_malformed_returns_decode_error() {
    match decode_line(b"not json\n") {
        Err(ParseError::Decode(_)) => {}
        other => panic!("expected Decode error, got {other:?}"),
    }
}

#[test]
fn decode_line_misrouted_request() {
    let req = DaemonRequest {
        id: 1,
        method: "system.status".into(),
        params: json!(null),
    };
    let bytes = encode_line(&req);
    match decode_line(&bytes) {
        Err(ParseError::MisroutedRequest) => {}
        other => panic!("expected MisroutedRequest, got {other:?}"),
    }
}

#[test]
fn decode_request_line_roundtrip() {
    let req = DaemonRequest {
        id: 99,
        method: "system.handshake".into(),
        params: json!({"protocol_version": 1}),
    };
    let bytes = encode_line(&req);
    let decoded = decode_request_line(&bytes).unwrap();
    assert_eq!(decoded, req);
}

// --- Phase 3: method dispatch ---

#[test]
fn method_try_from_status() {
    let req = DaemonRequest {
        id: 1,
        method: "system.status".into(),
        params: json!(null),
    };
    assert_eq!(Method::try_from(&req).unwrap(), Method::Status);
}

#[test]
fn method_try_from_status_empty_object() {
    let req = DaemonRequest {
        id: 1,
        method: "system.status".into(),
        params: json!({}),
    };
    assert_eq!(Method::try_from(&req).unwrap(), Method::Status);
}

#[test]
fn method_try_from_status_unexpected_params() {
    let req = DaemonRequest {
        id: 1,
        method: "system.status".into(),
        params: json!({"unexpected": true}),
    };
    match Method::try_from(&req) {
        Err(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[test]
fn method_try_from_handshake() {
    let req = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: json!({"protocol_version": 1}),
    };
    assert_eq!(
        Method::try_from(&req).unwrap(),
        Method::Handshake(HandshakeParams {
            protocol_version: 1,
            session_id: None,
        })
    );
}

#[test]
fn method_try_from_handshake_with_session_id() {
    let req = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: json!({"protocol_version": 1, "session_id": "20260424-150000"}),
    };
    assert_eq!(
        Method::try_from(&req).unwrap(),
        Method::Handshake(HandshakeParams {
            protocol_version: 1,
            session_id: Some("20260424-150000".into()),
        })
    );
}

#[test]
fn method_try_from_bogus_method() {
    let req = DaemonRequest {
        id: 1,
        method: "bogus.method".into(),
        params: json!(null),
    };
    match Method::try_from(&req) {
        Err(RpcError::MethodNotFound(m)) => assert_eq!(m, "bogus.method"),
        other => panic!("expected MethodNotFound, got {other:?}"),
    }
}

#[test]
fn method_try_from_handshake_deny_unknown_fields() {
    let req = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: json!({"protocol_version": 1, "bonus": "evil"}),
    };
    match Method::try_from(&req) {
        Err(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

// --- Phase 4: integration round-trips ---

#[test]
fn e2e_handshake_roundtrip() {
    let req = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: serde_json::to_value(HandshakeParams {
            protocol_version: PROTOCOL_VERSION,
            session_id: None,
        })
        .unwrap(),
    };
    let bytes = encode_line(&req);
    let decoded_req = decode_request_line(&bytes).unwrap();
    let method = Method::try_from(&decoded_req).unwrap();
    assert_eq!(
        method,
        Method::Handshake(HandshakeParams {
            protocol_version: 1,
            session_id: None,
        })
    );

    let result = serde_json::to_value(HandshakeResult {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").into(),
    })
    .unwrap();
    let resp = DaemonResponse::ok(decoded_req.id, result);
    let resp_bytes = encode_line(&resp);
    match decode_line(&resp_bytes).unwrap() {
        IpcMessage::Response(r) => {
            assert_eq!(r.id, 1);
            assert!(!r.is_error());
        }
        IpcMessage::Event(_) => panic!("expected Response"),
    }
}

#[test]
fn e2e_status_roundtrip() {
    let req = DaemonRequest {
        id: 2,
        method: "system.status".into(),
        params: json!(null),
    };
    let bytes = encode_line(&req);
    let decoded_req = decode_request_line(&bytes).unwrap();
    assert_eq!(Method::try_from(&decoded_req).unwrap(), Method::Status);

    let result = serde_json::to_value(StatusResult {
        started_at: "2026-04-19T00:00:00Z".into(),
        pid: 42,
        active_plans: 0,
        active_works: 0,
    })
    .unwrap();
    let resp = DaemonResponse::ok(decoded_req.id, result);
    let resp_bytes = encode_line(&resp);
    match decode_line(&resp_bytes).unwrap() {
        IpcMessage::Response(r) => assert_eq!(r.id, 2),
        IpcMessage::Event(_) => panic!("expected Response"),
    }
}

// --- Stage 5: plan.create method dispatch + result serde ---

#[test]
fn method_try_from_plan_create() {
    let req = DaemonRequest {
        id: 1,
        method: "plan.create".into(),
        params: json!({"goal": "ship v5"}),
    };
    assert_eq!(
        Method::try_from(&req).unwrap(),
        Method::PlanCreate(PlanCreateParams { goal: "ship v5".into() })
    );
}

#[test]
fn method_try_from_plan_create_missing_goal_is_invalid_params() {
    let req = DaemonRequest {
        id: 1,
        method: "plan.create".into(),
        params: json!({}),
    };
    match Method::try_from(&req) {
        Err(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[test]
fn method_try_from_plan_create_deny_unknown_fields() {
    let req = DaemonRequest {
        id: 1,
        method: "plan.create".into(),
        params: json!({"goal": "x", "bonus": "evil"}),
    };
    match Method::try_from(&req) {
        Err(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[test]
fn plan_create_result_roundtrip() {
    let plan = Plan::new("round-trip me".into());
    let before = PlanCreateResult { plan: plan.clone() };
    let bytes = serde_json::to_vec(&before).unwrap();
    let after: PlanCreateResult = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(after.plan.id, before.plan.id);
    assert_eq!(after.plan.goal, before.plan.goal);
    assert_eq!(after.plan.status, before.plan.status);
    assert_eq!(after.plan.created_at, before.plan.created_at);
    assert_eq!(after.plan.updated_at, before.plan.updated_at);
}

#[test]
fn plan_create_result_deny_unknown_fields() {
    let bytes = br#"{"plan": {"id": "pl-aaaaa", "updated_at": 0, "created_at": 0, "goal": "g", "status": "active"}, "bonus": "evil"}"#;
    assert!(
        serde_json::from_slice::<PlanCreateResult>(bytes).is_err(),
        "deny_unknown_fields must reject extra field"
    );
}

#[test]
fn plan_create_method_name_wire_form() {
    let name: &'static str = crate::method::MethodName::PlanCreate.into();
    assert_eq!(name, "plan.create");
    let name: &'static str = crate::method::MethodName::RecordList.into();
    assert_eq!(name, "record.list");
    let name: &'static str = crate::method::MethodName::RecordGet.into();
    assert_eq!(name, "record.get");
}

// --- record.list / record.get (Phase 3 of CLI plumbing shape) ---

#[test]
fn method_try_from_record_list() {
    let req = DaemonRequest {
        id: 3,
        method: "record.list".into(),
        params: json!({"kind": "plan"}),
    };
    assert_eq!(
        Method::try_from(&req).unwrap(),
        Method::RecordList(RecordListParams { kind: RecordKind::Plan })
    );
}

#[test]
fn method_try_from_record_list_every_kind() {
    for (wire, kind) in [
        ("plan", RecordKind::Plan),
        ("work", RecordKind::Work),
        ("bundle", RecordKind::Bundle),
        ("tick", RecordKind::Tick),
    ] {
        let req = DaemonRequest {
            id: 1,
            method: "record.list".into(),
            params: json!({"kind": wire}),
        };
        assert_eq!(
            Method::try_from(&req).unwrap(),
            Method::RecordList(RecordListParams { kind }),
            "kind={wire}"
        );
    }
}

#[test]
fn method_try_from_record_list_missing_kind_is_invalid_params() {
    let req = DaemonRequest {
        id: 3,
        method: "record.list".into(),
        params: json!({}),
    };
    match Method::try_from(&req) {
        Err(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[test]
fn method_try_from_record_list_bad_kind_is_invalid_params() {
    let req = DaemonRequest {
        id: 3,
        method: "record.list".into(),
        params: json!({"kind": "bogus"}),
    };
    match Method::try_from(&req) {
        Err(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[test]
fn method_try_from_record_get() {
    let req = DaemonRequest {
        id: 4,
        method: "record.get".into(),
        params: json!({"id": "pl-abcde"}),
    };
    assert_eq!(
        Method::try_from(&req).unwrap(),
        Method::RecordGet(RecordGetParams { id: "pl-abcde".into() })
    );
}

#[test]
fn records_result_plans_empty_roundtrip() {
    let before = RecordsResult::Plans(Vec::new());
    let bytes = serde_json::to_vec(&before).unwrap();
    let after: RecordsResult = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(after, before);
}

#[test]
fn records_result_wire_shape_is_adjacent_tagged() {
    let before = RecordsResult::Plans(Vec::new());
    let v = serde_json::to_value(&before).unwrap();
    let obj = v.as_object().expect("object");
    assert_eq!(obj.get("kind").and_then(|k| k.as_str()), Some("plans"));
    assert!(
        obj.get("records").is_some(),
        "adjacent tagging nests the value under `records`"
    );
}

#[test]
fn records_result_plans_roundtrip_with_summaries() {
    let p1 = Plan::new("first".into());
    let p2 = Plan::new("second".into());
    let summaries = vec![PlanSummary::from(&p1), PlanSummary::from(&p2)];
    let before = RecordsResult::Plans(summaries);
    let bytes = serde_json::to_vec(&before).unwrap();
    let after: RecordsResult = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(after, before);
    match after {
        RecordsResult::Plans(ps) => {
            assert_eq!(ps.len(), 2);
            assert_eq!(ps[0].id, p1.id);
            assert_eq!(ps[1].id, p2.id);
        }
        other => panic!("expected Plans, got {other:?}"),
    }
}

#[test]
fn record_result_plan_roundtrip() {
    let plan = Plan::new("one".into());
    let before = RecordResult::Plan(plan.clone());
    let bytes = serde_json::to_vec(&before).unwrap();
    let after: RecordResult = serde_json::from_slice(&bytes).unwrap();
    match after {
        RecordResult::Plan(p) => {
            assert_eq!(p.id, plan.id);
            assert_eq!(p.goal, plan.goal);
        }
        other => panic!("expected Plan, got {other:?}"),
    }
}

#[test]
fn plan_summary_projection_is_lossy_but_preserves_identity() {
    let plan = Plan::new("coverage".into());
    let summary = PlanSummary::from(&plan);
    assert_eq!(summary.id, plan.id);
    assert_eq!(summary.goal, plan.goal);
    assert_eq!(summary.status, plan.status);
    assert_eq!(summary.updated_at, plan.updated_at);
}

#[test]
fn record_kind_wire_form_is_kebab_case() {
    assert_eq!(serde_json::to_value(RecordKind::Plan).unwrap(), json!("plan"));
    assert_eq!(serde_json::to_value(RecordKind::Work).unwrap(), json!("work"));
    assert_eq!(serde_json::to_value(RecordKind::Bundle).unwrap(), json!("bundle"));
    assert_eq!(serde_json::to_value(RecordKind::Tick).unwrap(), json!("tick"));
}

#[test]
fn records_result_bundles_frame_fits_under_max_line_bytes() {
    // A pathological list of 1000 BundleSummaries should land well below
    // the 1 MiB IPC frame cap. If this test starts failing, summaries
    // grew too large (or a field was added that shouldn't be in the
    // projection) and the frame-cap risk flagged in the design doc is
    // live.
    let mut bundles = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let b = domain::Bundle::new(domain::WorkId::new(), "loopr/wk-xxxxxxx-1".into(), vec!["claim".into()]);
        bundles.push(BundleSummary::from(&b));
    }
    let result = RecordsResult::Bundles(bundles);
    let bytes = serde_json::to_vec(&result).unwrap();
    assert!(
        bytes.len() < crate::MAX_LINE_BYTES,
        "1000 BundleSummaries encoded to {} bytes, over the {} MiB cap",
        bytes.len(),
        crate::MAX_LINE_BYTES / (1 << 20)
    );
    // Also assert a comfortable margin so we don't pass right at the cliff.
    assert!(
        bytes.len() < crate::MAX_LINE_BYTES / 2,
        "1000 BundleSummaries encoded to {} bytes; expected well under 512 KiB",
        bytes.len()
    );
}

#[test]
fn e2e_event_roundtrip() {
    let event = DaemonEvent {
        event: "tick.published".into(),
        data: json!({"tick-id": "t1"}),
    };
    let bytes = encode_line(&event);
    match decode_line(&bytes).unwrap() {
        IpcMessage::Event(e) => assert_eq!(e, event),
        IpcMessage::Response(_) => panic!("expected Event"),
    }
}

#[test]
fn version_advertisement_in_handshake_bytes() {
    let req = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: serde_json::to_value(HandshakeParams {
            protocol_version: PROTOCOL_VERSION,
            session_id: None,
        })
        .unwrap(),
    };
    let bytes = encode_line(&req);
    let s = std::str::from_utf8(&bytes).unwrap();
    assert!(
        s.contains("\"protocol_version\":1"),
        "expected protocol_version:1 in: {s}"
    );
}
