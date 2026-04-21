use serde_json::json;

use domain::Plan;

use crate::envelope::{DaemonEvent, DaemonRequest, DaemonResponse};
use crate::error::RpcError;
use crate::frame::{ParseError, decode_line, decode_request_line, encode_line};
use crate::method::{
    HandshakeParams, HandshakeResult, Method, PlanCreateParams, PlanCreateResult, PlanListResult, StatusResult,
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
        Method::Handshake(HandshakeParams { protocol_version: 1 })
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
        })
        .unwrap(),
    };
    let bytes = encode_line(&req);
    let decoded_req = decode_request_line(&bytes).unwrap();
    let method = Method::try_from(&decoded_req).unwrap();
    assert_eq!(method, Method::Handshake(HandshakeParams { protocol_version: 1 }));

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

// --- Stage 5: plan.create / plan.list method dispatch + result serde ---

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
fn method_try_from_plan_list_null_params() {
    let req = DaemonRequest {
        id: 2,
        method: "plan.list".into(),
        params: json!(null),
    };
    assert_eq!(Method::try_from(&req).unwrap(), Method::PlanList);
}

#[test]
fn method_try_from_plan_list_empty_object_params() {
    let req = DaemonRequest {
        id: 2,
        method: "plan.list".into(),
        params: json!({}),
    };
    assert_eq!(Method::try_from(&req).unwrap(), Method::PlanList);
}

#[test]
fn method_try_from_plan_list_unexpected_params() {
    let req = DaemonRequest {
        id: 2,
        method: "plan.list".into(),
        params: json!({"filter": "x"}),
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
fn plan_list_result_roundtrip_empty() {
    let before = PlanListResult { plans: Vec::new() };
    let bytes = serde_json::to_vec(&before).unwrap();
    let after: PlanListResult = serde_json::from_slice(&bytes).unwrap();
    assert!(after.plans.is_empty());
}

#[test]
fn plan_list_result_roundtrip_preserves_order() {
    let p1 = Plan::new("first".into());
    let p2 = Plan::new("second".into());
    let before = PlanListResult {
        plans: vec![p1.clone(), p2.clone()],
    };
    let bytes = serde_json::to_vec(&before).unwrap();
    let after: PlanListResult = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(after.plans.len(), 2);
    assert_eq!(after.plans[0].id, p1.id);
    assert_eq!(after.plans[1].id, p2.id);
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
    let name: &'static str = crate::method::MethodName::PlanList.into();
    assert_eq!(name, "plan.list");
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
