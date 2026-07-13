//! `director.chat` IPC handler tests (Phase 8 of director-phase-2): the
//! `truncate_chat_message` byte-cap / UTF-8-boundary unit tests plus the
//! missing-plan and oversized-message dispatch paths. Split out of the parent
//! `tests.rs` so it stays under the per-file line limit; shares the parent's
//! `stub_ctx` fixture via `super::`.

use std::str::FromStr;

use ipc::{DaemonRequest, RpcError};

use super::super::{HandshakeState, dispatch, truncate_chat_message};
use super::stub_ctx;

// ---------- Phase 8: director.chat (truncation + missing-plan path) ----------

#[test]
fn truncate_chat_message_passes_through_short_payload() {
    let small = "hello".repeat(10);
    assert!(small.len() < ipc::DIRECTOR_CHAT_MESSAGE_BYTE_CAP);
    let out = truncate_chat_message(&small);
    assert_eq!(out, small, "below-cap payload must pass through unchanged");
}

#[test]
fn truncate_chat_message_clips_oversized_payload_with_marker() {
    let huge = "a".repeat(5 * 1024);
    let out = truncate_chat_message(&huge);
    assert!(out.len() < huge.len(), "oversized payload must shrink");
    assert!(
        out.starts_with(&"a".repeat(ipc::DIRECTOR_CHAT_MESSAGE_BYTE_CAP)),
        "first 4 KB must be preserved verbatim"
    );
    assert!(
        out.contains("[truncated: original 5120 bytes]"),
        "marker must include the original byte count: {out}"
    );
}

#[test]
fn truncate_chat_message_respects_utf8_char_boundaries() {
    // Build a payload whose 4096th byte falls mid-codepoint (uses `é`
    // which is 2 bytes). The truncation must retreat to the previous
    // char boundary rather than panicking on an invalid slice.
    let one_é = "é"; // 2 bytes
    let mut payload = "a".repeat(4095);
    payload.push_str(one_é); // payload.len() == 4097; cap is 4096
    let out = truncate_chat_message(&payload);
    // The retained prefix is the first 4094 bytes — 4095 minus one to
    // avoid splitting the multi-byte `é`.
    assert!(out.starts_with(&"a".repeat(4094)));
    assert!(out.is_char_boundary(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn director_chat_nonexistent_plan_yields_not_found() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 50,
        method: "director.chat".into(),
        params: serde_json::json!({
            "plan_id": "pl-zzzzz",
            "message": "hi"
        }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 50);
    match resp.error {
        Some(RpcError::NotFound(msg)) => {
            assert!(msg.contains("pl-zzzzz"), "expected plan id in NotFound: {msg}");
        }
        other => panic!("expected NotFound for missing plan; got {other:?}"),
    }
    assert!(resp.result.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn director_chat_oversized_message_is_truncated_in_store() {
    let (_td, ctx) = stub_ctx().await;
    // Seed a Plan via the store directly so we don't run the full
    // plan.create LLM-driven path.
    let plan = domain::Plan::new("phase-8-truncation-target".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let huge = "a".repeat(5 * 1024);
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 51,
        method: "director.chat".into(),
        params: serde_json::json!({
            "plan_id": plan_id.to_string(),
            "message": huge,
        }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 51);
    assert!(
        resp.error.is_none(),
        "director.chat with valid plan must succeed: {:?}",
        resp.error
    );
    let result_value = resp.result.expect("result");
    let result: ipc::DirectorChatResult = serde_json::from_value(result_value).unwrap();

    // The persisted note must carry the truncated payload, not the
    // full 5 KB. Confirms the truncation happens BEFORE persist.
    let note_id = domain::NoteId::from_str(&result.note_id).unwrap();
    let stored = ctx.store.notes().get(&note_id).await.unwrap();
    assert!(stored.message.len() < huge.len(), "stored note must be truncated");
    assert!(stored.message.contains("[truncated: original 5120 bytes]"));
    assert_eq!(stored.plan_id, plan_id);
}
