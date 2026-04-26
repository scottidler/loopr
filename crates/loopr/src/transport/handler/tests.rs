#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tempfile::TempDir;

use agents::{ImplementerConfig, ReviewerConfig};
use context::InlineContextBuilder;
use ipc::{
    DaemonRequest, HandshakeParams, HandshakeResult, PROTOCOL_VERSION, PlanCreateResult, RecordKind, RecordListParams,
    RecordsResult, RpcError, StatusResult,
};
use llm::{AnthropicClient, LlmConfig};
use store::Store;
use telemetry::{ProcessId, SessionId};
use tools::{BashDenylist, LaneRouter, SandboxMode};
use worktree::AttemptCleanupPolicy;

use super::*;
use crate::daemon::DaemonContext;

/// Build a dummy `AnthropicClient` whose `api_base_url` points at a
/// non-listening local port so any actual call fails fast. Lets
/// handler tests exercise the Plan-persistence path without hitting
/// real Anthropic.
fn dummy_anthropic() -> Arc<AnthropicClient> {
    let cfg = LlmConfig {
        api_base_url: "http://127.0.0.1:1".to_string(),
        ..LlmConfig::default()
    };
    let client = AnthropicClient::new(cfg, "test-key".to_string()).expect("dummy anthropic");
    Arc::new(client)
}

/// Test context backed by a real `Store` rooted at a `TempDir`. Callers
/// keep the `TempDir` alive for the life of the test so the store's
/// on-disk files outlive its in-process operations.
async fn stub_ctx() -> (TempDir, Arc<DaemonContext<AnthropicClient>>) {
    let td = TempDir::new().unwrap();
    init_git_repo(td.path());
    let store = Store::open(td.path()).await.unwrap();
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
    let bash_denylist = Arc::new(BashDenylist::with_base());
    let snapshot = Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
        "test-stub-model",
    )));
    let ctx = Arc::new(DaemonContext::new(
        td.path().to_path_buf(),
        SessionId::parse("20260419-000000").unwrap(),
        "-test-target".to_string(),
        ProcessId::parse("pc-test01").unwrap(),
        12345,
        store,
        dummy_anthropic(),
        router,
        bash_denylist,
        Vec::new(),
        SandboxMode::Off,
        Arc::new(InlineContextBuilder::new()),
        ImplementerConfig::default(),
        ReviewerConfig::default(),
        integrator::IntegratorConfig::default(),
        AttemptCleanupPolicy::default(),
        snapshot,
    ));
    (td, ctx)
}

/// Initialize a git repo at `path` with a single empty commit so HEAD
/// exists. `handle_plan_create` calls `ensure_integration_branch`, which
/// needs a valid HEAD to branch from; a bare tempdir has neither.
fn init_git_repo(path: &std::path::Path) {
    use std::process::Command;
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["config", "tag.gpgsign", "false"]);
    run(&["commit", "--allow-empty", "-q", "-m", "initial"]);
}

fn handshake_req(id: u64, version: u32) -> DaemonRequest {
    DaemonRequest {
        id,
        method: "system.handshake".into(),
        params: serde_json::to_value(HandshakeParams {
            protocol_version: version,
            session_id: None,
        })
        .unwrap(),
    }
}

fn status_req(id: u64) -> DaemonRequest {
    DaemonRequest {
        id,
        method: "system.status".into(),
        params: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn handshake_completes_state() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Pending;
    let resp = dispatch(&handshake_req(1, PROTOCOL_VERSION), &mut state, &ctx).await;
    assert_eq!(resp.id, 1);
    assert!(resp.error.is_none(), "no error: {resp:?}");
    let result: HandshakeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.protocol_version, PROTOCOL_VERSION);
    assert_eq!(state, HandshakeState::Complete);
}

#[tokio::test]
async fn handshake_mismatch_returns_protocol_version_mismatch() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Pending;
    let resp = dispatch(&handshake_req(2, 99), &mut state, &ctx).await;
    assert_eq!(resp.id, 2);
    match resp.error {
        Some(RpcError::ProtocolVersionMismatch(msg)) => {
            assert!(msg.contains("client=99"), "msg: {msg}");
        }
        other => panic!("expected ProtocolVersionMismatch, got {other:?}"),
    }
    // State stays Pending; a subsequent correct handshake is allowed.
    assert_eq!(state, HandshakeState::Pending);
}

#[tokio::test]
async fn status_before_handshake_is_invalid_request() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Pending;
    let resp = dispatch(&status_req(3), &mut state, &ctx).await;
    assert_eq!(resp.id, 3);
    match resp.error {
        Some(RpcError::InvalidRequest(msg)) => {
            assert!(msg.contains("handshake required"), "msg: {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn status_after_handshake_returns_status_result() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Pending;
    dispatch(&handshake_req(4, PROTOCOL_VERSION), &mut state, &ctx).await;
    let resp = dispatch(&status_req(5), &mut state, &ctx).await;
    assert_eq!(resp.id, 5);
    assert!(resp.error.is_none());
    let result: StatusResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.pid, ctx.pid);
    assert_eq!(result.active_plans, 0);
    assert_eq!(result.active_works, 0);
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 6,
        method: "bogus.method".into(),
        params: serde_json::json!({"goal": "x"}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 6);
    match resp.error {
        Some(RpcError::MethodNotFound(m)) => assert_eq!(m, "bogus.method"),
        other => panic!("expected MethodNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn plan_create_persists_and_returns_plan() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 10,
        method: "plan.create".into(),
        params: serde_json::json!({"goal": "first goal"}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 10);
    assert!(resp.error.is_none(), "no error: {resp:?}");
    let result: PlanCreateResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.plan.goal, "first goal");
    assert!(result.plan.id.as_ref().starts_with("pl-"));

    // Seam verification: the store actually received the record.
    let listed = ctx.store.plans().list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].goal, "first goal");
}

/// With the dummy `AnthropicClient` (pointing at a non-listening
/// port) the decomposer's LLM call fails; the handler must still
/// return Plan success AND leave the works collection empty — not
/// partially populated, not errored-out.
#[tokio::test]
async fn plan_create_with_failing_llm_still_persists_plan_and_leaves_works_empty() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 100,
        method: "plan.create".into(),
        params: serde_json::json!({"goal": "will decompose-fail"}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 100);
    assert!(
        resp.error.is_none(),
        "plan.create returns Plan success even when decompose fails: {resp:?}"
    );

    let plans = ctx.store.plans().list().await.unwrap();
    assert_eq!(plans.len(), 1);
    let works = ctx.store.works().list().await.unwrap();
    assert!(works.is_empty(), "no Works persisted when decompose fails");
}

#[tokio::test]
async fn record_list_plans_returns_all_plan_summaries() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;

    // Seed two plans via the handler so the test exercises the store
    // through the dispatch path, not through the store API directly.
    for goal in ["first", "second"] {
        let req = DaemonRequest {
            id: 11,
            method: "plan.create".into(),
            params: serde_json::json!({"goal": goal}),
        };
        let resp = dispatch(&req, &mut state, &ctx).await;
        assert!(resp.error.is_none(), "seed create no error: {resp:?}");
    }

    let req = DaemonRequest {
        id: 12,
        method: "record.list".into(),
        params: serde_json::to_value(RecordListParams { kind: RecordKind::Plan }).unwrap(),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 12);
    assert!(resp.error.is_none(), "list no error: {resp:?}");
    let result: RecordsResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    match result {
        RecordsResult::Plans(summaries) => {
            assert_eq!(summaries.len(), 2);
            let goals: Vec<_> = summaries.iter().map(|p| p.goal.clone()).collect();
            assert!(goals.contains(&"first".to_string()));
            assert!(goals.contains(&"second".to_string()));
        }
        other => panic!("expected Plans, got {other:?}"),
    }
}

#[tokio::test]
async fn plan_create_bad_params_is_invalid_params() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 13,
        method: "plan.create".into(),
        params: serde_json::json!({"bad-field": "x"}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 13);
    match resp.error {
        Some(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[tokio::test]
async fn record_list_rejects_missing_kind() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 14,
        method: "record.list".into(),
        params: serde_json::json!({}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 14);
    match resp.error {
        Some(RpcError::InvalidParams(_)) => {}
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

// RFC-8259 newline-safety: a literal `\n` inside a string field must not
// split the NDJSON line prematurely. `serde_json::to_string` ASCII-
// escapes `\n` to `\\n` in compact mode, so the only `0x0A` on the wire
// is the terminator appended by the framing layer.
#[test]
fn serde_json_compact_escapes_embedded_newlines() {
    let obj = serde_json::json!({"message": "line1\nline2"});
    let compact = serde_json::to_string(&obj).unwrap();
    // Wire form contains escaped `\\n`, NOT a bare 0x0A.
    assert!(compact.contains("\\n"), "compact: {compact}");
    assert!(!compact.contains('\n'), "no bare 0x0A on wire: {compact}");
    // Round-trip.
    let back: serde_json::Value = serde_json::from_str(&compact).unwrap();
    assert_eq!(back, obj);
}
