#![allow(clippy::unwrap_used)]

use std::str::FromStr;
use std::sync::Arc;

use tempfile::TempDir;

use agents::{DirectorConfig, ImplementerConfig, ReviewerConfig};
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
        DirectorConfig::default(),
        decomposer::DecomposerConfig::default(),
        AttemptCleanupPolicy::default(),
        snapshot,
        crate::transport::ServerTimeouts::default(),
        None,
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
    // Empty store: nothing active yet.
    assert_eq!(result.active_plans, 0);
    assert_eq!(result.active_works, 0);
}

#[tokio::test]
async fn status_counts_active_plans_and_works() {
    let (_td, ctx) = stub_ctx().await;
    // One Active Plan (Plan::new births Active) with two non-terminal Works
    // and one Done Work — active_works counts only the non-terminal pair.
    let plan = domain::Plan::new("count me".to_string());
    ctx.store.plans().create(plan.clone()).await.unwrap();
    let w1 = domain::Work::new(plan.id.clone(), "w1".to_string());
    let w2 = domain::Work::new(plan.id.clone(), "w2".to_string());
    let mut w3 = domain::Work::new(plan.id.clone(), "w3".to_string());
    w3.status = domain::WorkStatus::Done;
    ctx.store.works().create(w1).await.unwrap();
    ctx.store.works().create(w2).await.unwrap();
    ctx.store.works().create(w3).await.unwrap();

    let mut state = HandshakeState::Pending;
    dispatch(&handshake_req(6, PROTOCOL_VERSION), &mut state, &ctx).await;
    let resp = dispatch(&status_req(7), &mut state, &ctx).await;
    let result: StatusResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.active_plans, 1, "one Active plan");
    assert_eq!(result.active_works, 2, "two non-terminal works (Done excluded)");
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

    // Phase B (async ACK): decompose now runs on a `plan_create_tasks`
    // task. Drain it so we assert the post-decompose state (decompose ran
    // and failed) rather than racing the spawn before it starts.
    {
        let mut tasks = ctx.plan_create_tasks.lock().await;
        while tasks.join_next().await.is_some() {}
    }

    let works = ctx.store.works().list().await.unwrap();
    assert!(works.is_empty(), "no Works persisted when decompose fails");

    // Decompose-failure visibility (Architect audit follow-up): the Plan is
    // transitioned to Stalled so the operator sees a stuck Plan via
    // `loopr plans` instead of a deceptive Active Plan with zero Works.
    let plans = ctx.store.plans().list().await.unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].status,
        domain::PlanStatus::Stalled,
        "decompose failure must leave the Plan visibly Stalled, not Active"
    );
}

/// `decompose_and_dispatch` unconditionally spawns a per-Plan Director
/// alongside the Implementer, and both draw `complete_free` calls from
/// the SAME underlying `ScriptedLlm` FIFO queue when wrapped directly —
/// racing the single queued Implementer response against the Director's
/// own first iteration. This wrapper (mirrors
/// `tests/stage_8_plan_to_tick.rs::Stage8DirectorAwareLlm`) shields the
/// Director's model="claude-opus-4-7" calls from the queue entirely by
/// always answering `done` (no Bundle exists yet in this scenario, so
/// there is nothing for the Director to act on); every other call
/// (decomposer, Implementer) passes through to the inner `ScriptedLlm`
/// untouched, so the queue is exclusively the Implementer's.
#[derive(Clone)]
struct DirectorNoopLlm {
    inner: llm::ScriptedLlm,
}

impl llm::LlmClient for DirectorNoopLlm {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: llm::ToolSchema,
        model: Option<&str>,
    ) -> Result<(llm::ToolCall, llm::Usage), llm::LlmError> {
        self.inner.complete_with_tool(system, user, tool, model).await
    }

    async fn complete_free(
        &self,
        system: &str,
        messages: &[llm::Message],
        model: Option<&str>,
    ) -> Result<(String, llm::Usage), llm::LlmError> {
        if model == Some("claude-opus-4-7") {
            return Ok((
                r#"[{"action":"done","summary":"test: no bundle to act on"}]"#.to_string(),
                llm::Usage::default(),
            ));
        }
        self.inner.complete_free(system, messages, model).await
    }

    fn model(&self) -> &str {
        self.inner.model()
    }
}

/// Test context identical to `stub_ctx()` but backed by a `ScriptedLlm`
/// (wrapped in `DirectorNoopLlm`) instead of a dead-port `AnthropicClient`,
/// so decompose can actually succeed and drive Work creation. Used by the
/// Phase 4 pipeline-counter e2e test below.
async fn stub_ctx_scripted(llm: llm::ScriptedLlm) -> (TempDir, Arc<DaemonContext<DirectorNoopLlm>>) {
    let td = TempDir::new().unwrap();
    init_git_repo(td.path());
    let store = Store::open(td.path()).await.unwrap();
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
    let bash_denylist = Arc::new(BashDenylist::with_base());
    let snapshot = Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
        "test-scripted-model",
    )));
    let ctx = Arc::new(DaemonContext::new(
        td.path().to_path_buf(),
        SessionId::parse("20260712-000002").unwrap(),
        "-test-scripted".to_string(),
        ProcessId::parse("pc-scrpt1").unwrap(),
        12346,
        store,
        Arc::new(DirectorNoopLlm { inner: llm }),
        router,
        bash_denylist,
        Vec::new(),
        SandboxMode::Off,
        Arc::new(InlineContextBuilder::new()),
        ImplementerConfig::default(),
        ReviewerConfig::default(),
        integrator::IntegratorConfig::default(),
        DirectorConfig::default(),
        decomposer::DecomposerConfig::default(),
        AttemptCleanupPolicy::default(),
        snapshot,
        crate::transport::ServerTimeouts::default(),
        None,
    ));
    (td, ctx)
}

/// Phase 4 success criterion: "process digest shows nonzero counters
/// after a scripted e2e." Drives `plan.create` over the real dispatch
/// path with a scripted decomposer (one Work, no deps) and a scripted
/// Implementer that immediately escalates (`need_help`) — the escalation
/// keeps this test self-contained (no Reviewer/Integrator stub responses
/// needed) while still exercising every counter this phase wires:
/// `plans_created` (Plan persisted), `works_created` (batch persisted by
/// decompose), and `works_blocked` (the Implementer's `NeedHelp` action
/// blocks the Work via `transition_and_persist_work`).
#[tokio::test]
async fn plan_create_scripted_e2e_increments_process_digest_counters() {
    let scripted = llm::ScriptedLlm::new();
    scripted.queue_tool(Ok(llm::ToolCall {
        tool_name: "submit_decomposition".to_string(),
        input: serde_json::json!({
            "children": [
                {
                    "title": "Add VERSION.md",
                    "content": "Create a top-level VERSION.md file.",
                    "dependencies": [],
                    "acceptance_criteria": ["VERSION.md exists at repo root"],
                    "files": ["VERSION.md"],
                }
            ]
        }),
    }));
    scripted.queue_free(Ok(r#"[{"type":"need_help","reason":"test escalation"}]"#.to_string()));

    let (_td, ctx) = stub_ctx_scripted(scripted).await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 200,
        method: "plan.create".into(),
        params: serde_json::json!({"goal": "add VERSION.md"}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert!(resp.error.is_none(), "plan.create errored: {:?}", resp.error);

    // Drain the async-ACK decompose task (increments plans_created before
    // this call even starts; increments works_created inside it).
    {
        let mut tasks = ctx.plan_create_tasks.lock().await;
        while tasks.join_next().await.is_some() {}
    }
    // Drain the Implementer task the decompose spawned (increments
    // works_blocked on the NeedHelp escalation). The per-Plan Director
    // task is deliberately NOT drained here: `DirectorConfig::default()`
    // idles 15s between iterations, and cutting that sleep short needs
    // either a zeroed poll/idle interval or a `shutdown_notify` wakeup
    // timed against the director's select point — neither is needed for
    // this test's assertions. The test's own single-threaded tokio
    // runtime aborts the still-sleeping Director task when it tears down
    // at the end of this function, same as any other detached task.
    {
        let mut tasks = ctx.implementer_tasks.lock().await;
        while tasks.join_next().await.is_some() {}
    }

    let snap = ctx.snapshot.lock().unwrap();
    assert_eq!(snap.plans_created, 1, "plan.create must increment plans_created");
    assert_eq!(snap.works_created, 1, "decompose must increment works_created");
    assert_eq!(
        snap.works_blocked, 1,
        "the Implementer's NeedHelp escalation must increment works_blocked"
    );
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

// ---------- Phase 8: director.chat (truncation + missing-plan path) ----------

#[test]
fn truncate_chat_message_passes_through_short_payload() {
    let small = "hello".repeat(10);
    assert!(small.len() < ipc::DIRECTOR_CHAT_MESSAGE_BYTE_CAP);
    let out = super::truncate_chat_message(&small);
    assert_eq!(out, small, "below-cap payload must pass through unchanged");
}

#[test]
fn truncate_chat_message_clips_oversized_payload_with_marker() {
    let huge = "a".repeat(5 * 1024);
    let out = super::truncate_chat_message(&huge);
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
    let out = super::truncate_chat_message(&payload);
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

// --- plan.override (Phase 10 of director-phase-2) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_override_stalled_to_active_persists_and_responds() {
    let (_td, ctx) = stub_ctx().await;
    // Seed a Stalled Plan directly.
    let mut plan = domain::Plan::new("phase-10-override-target".to_string());
    plan.status = domain::PlanStatus::Stalled;
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 60,
        method: "plan.override".into(),
        params: serde_json::json!({
            "plan_id": plan_id.to_string(),
            "target_status": "active",
        }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 60);
    assert!(
        resp.error.is_none(),
        "plan.override Stalled -> Active must succeed: {:?}",
        resp.error
    );
    let result_value = resp.result.expect("result");
    let result: ipc::PlanOverrideResult = serde_json::from_value(result_value).unwrap();
    assert_eq!(result.plan.status, domain::PlanStatus::Active);

    // Persisted Plan reflects the new status.
    let after = ctx.store.plans().get(&plan_id).await.unwrap();
    assert_eq!(after.status, domain::PlanStatus::Active);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_override_nonexistent_plan_yields_not_found() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 61,
        method: "plan.override".into(),
        params: serde_json::json!({
            "plan_id": "pl-zzzzz",
            "target_status": "active",
        }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 61);
    assert!(matches!(resp.error, Some(RpcError::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_override_unknown_target_status_yields_invalid_params() {
    let (_td, ctx) = stub_ctx().await;
    let plan = domain::Plan::new("status-parser-target".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 62,
        method: "plan.override".into(),
        params: serde_json::json!({
            "plan_id": plan_id.to_string(),
            "target_status": "FROBNICATE",
        }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 62);
    match resp.error {
        Some(RpcError::InvalidParams(msg)) => {
            assert!(msg.contains("FROBNICATE"), "expected status string in error: {msg}");
        }
        other => panic!("expected InvalidParams for bogus status; got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_override_rejected_fsm_edge_yields_invalid_request() {
    // Active -> Complete is allowed only for `(Reactor, Decomposer)`
    // per the Plan FSM transition table — `Role::Director` is not in
    // the role list. The handler must surface the FSM rejection as
    // InvalidRequest, not panic, and must not mutate the Plan.
    let (_td, ctx) = stub_ctx().await;
    let plan = domain::Plan::new("fsm-reject-target".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 63,
        method: "plan.override".into(),
        params: serde_json::json!({
            "plan_id": plan_id.to_string(),
            "target_status": "complete",
        }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 63);
    match resp.error {
        Some(RpcError::InvalidRequest(msg)) => {
            // F8: the override now routes through transition_and_persist_plan,
            // whose rejection message is "...fsm override rejected...".
            assert!(
                msg.to_lowercase().contains("fsm"),
                "expected FSM rejection in error: {msg}"
            );
        }
        other => panic!("expected InvalidRequest for FSM rejection; got {other:?}"),
    }
    // Plan must NOT have been mutated.
    let after = ctx.store.plans().get(&plan_id).await.unwrap();
    assert_eq!(after.status, domain::PlanStatus::Active);
}

// --- director.status (Phase 2 follow-ups, Item 3) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn director_status_nonexistent_plan_yields_not_found() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 70,
        method: "director.status".into(),
        params: serde_json::json!({ "plan_id": "pl-zzzzz" }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 70);
    match resp.error {
        Some(RpcError::NotFound(msg)) => {
            assert!(msg.contains("pl-zzzzz"), "expected plan id in NotFound: {msg}");
        }
        other => panic!("expected NotFound for missing plan; got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn director_status_plan_exists_with_no_live_snapshot_returns_none() {
    let (_td, ctx) = stub_ctx().await;
    // Seed a Plan via the store directly; do NOT populate
    // `director_statuses` — simulates a Plan that's persisted but has
    // no live Director (Stalled / Complete / pre-spawn race).
    let plan = domain::Plan::new("status-no-snapshot-target".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 71,
        method: "director.status".into(),
        params: serde_json::json!({ "plan_id": plan_id.to_string() }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 71);
    assert!(resp.error.is_none(), "no-snapshot path must succeed: {:?}", resp.error);
    let result: ipc::DirectorStatusResult = serde_json::from_value(resp.result.expect("result")).unwrap();
    assert_eq!(result.plan_id, plan_id.to_string());
    assert!(
        result.snapshot.is_none(),
        "expected snapshot=None when no Director is live"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn director_status_plan_with_live_snapshot_returns_fields() {
    let (_td, ctx) = stub_ctx().await;
    let plan = domain::Plan::new("status-live-snapshot-target".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    // Inject a snapshot as if the Director had just finished an
    // iteration. The handler's read path is the same regardless of
    // who wrote.
    let snapshot = agents::DirectorStatusSnapshot {
        mode: agents::DirectorMode::Conservative,
        no_progress_streak: 3,
        same_action_streak: 2,
        iteration: 14,
        last_action_kind: Some("override_work".to_string()),
        last_action_target_id: Some("wk-xyz".to_string()),
        last_action_ts: Some(1_700_000_000_000),
        unread_note_count: 2,
        needs_operator_iters: 0,
    };
    ctx.director_statuses.write().unwrap().insert(plan_id.clone(), snapshot);

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 72,
        method: "director.status".into(),
        params: serde_json::json!({ "plan_id": plan_id.to_string() }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 72);
    assert!(resp.error.is_none(), "happy path must succeed: {:?}", resp.error);
    let result: ipc::DirectorStatusResult = serde_json::from_value(resp.result.expect("result")).unwrap();
    let wire = result.snapshot.expect("snapshot present");
    assert_eq!(wire.mode, "Conservative");
    assert_eq!(wire.no_progress_streak, 3);
    assert_eq!(wire.same_action_streak, 2);
    assert_eq!(wire.iteration, 14);
    assert_eq!(wire.last_action_kind.as_deref(), Some("override_work"));
    assert_eq!(wire.last_action_target_id.as_deref(), Some("wk-xyz"));
    assert_eq!(wire.last_action_ts, Some(1_700_000_000_000));
    assert_eq!(wire.unread_note_count, 2);
    assert_eq!(wire.needs_operator_iters, 0);
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

// --- record.get (dispatch routing + map_store_error arm coverage) ---

#[tokio::test]
async fn record_get_plan_returns_full_record() {
    let (_td, ctx) = stub_ctx().await;
    let plan = domain::Plan::new("record-get-target".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 80,
        method: "record.get".into(),
        params: serde_json::json!({ "id": plan_id.to_string() }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 80);
    assert!(resp.error.is_none(), "record.get happy path: {resp:?}");
    let result: ipc::RecordResult = serde_json::from_value(resp.result.expect("result")).unwrap();
    match result {
        ipc::RecordResult::Plan(p) => {
            assert_eq!(p.id, plan_id);
            assert_eq!(p.goal, "record-get-target");
        }
        other => panic!("expected RecordResult::Plan, got {other:?}"),
    }
}

#[tokio::test]
async fn record_get_unknown_prefix_yields_invalid_params() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 81,
        method: "record.get".into(),
        params: serde_json::json!({ "id": "zz-abcde" }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 81);
    match resp.error {
        Some(RpcError::InvalidParams(msg)) => {
            assert!(msg.contains("zz-abcde"), "expected id in error: {msg}");
        }
        other => panic!("expected InvalidParams for unknown prefix, got {other:?}"),
    }
}

#[tokio::test]
async fn record_get_nonexistent_id_yields_not_found() {
    let (_td, ctx) = stub_ctx().await;
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 82,
        method: "record.get".into(),
        params: serde_json::json!({ "id": "pl-zzzzz" }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 82);
    // Routes through store_err_response -> map_store_error(RecordNotFound).
    match resp.error {
        Some(RpcError::NotFound(msg)) => {
            assert!(
                msg.contains("plans/pl-zzzzz"),
                "expected collection/id in NotFound: {msg}"
            );
        }
        other => panic!("expected NotFound for missing plan, got {other:?}"),
    }
}

/// Phase 6 F6 (2026-07-11-verified-swarm): a `record.get` result that
/// serializes over the frame budget must be caught BEFORE the response
/// is handed to the wire codec, and surfaced as a typed
/// `RpcError::PayloadTooLarge` rather than a dropped connection. This
/// exercises `handle_record_get`'s length check directly; the
/// server-level round-trip proving the underlying connection survives
/// lives in `transport::server::tests::record_get_oversized_result_yields_typed_error_connection_survives`.
#[tokio::test]
async fn record_get_oversized_result_yields_payload_too_large() {
    let (_td, ctx) = stub_ctx().await;
    // `RECORD_GET_MAX_RESULT_BYTES` is `ipc::MAX_LINE_BYTES` minus a small
    // envelope margin; a goal string well past that guarantees the
    // serialized `RecordResult::Plan` alone exceeds the budget.
    let huge_goal = "x".repeat(ipc::MAX_LINE_BYTES);
    let plan = domain::Plan::new(huge_goal);
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 90,
        method: "record.get".into(),
        params: serde_json::json!({ "id": plan_id.to_string() }),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 90);
    match resp.error {
        Some(RpcError::PayloadTooLarge(msg)) => {
            assert!(msg.contains(plan_id.as_ref()), "expected record id in error: {msg}");
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    // The connection-survives contract at the handler layer: dispatching
    // again on the same context/handshake state (what one live connection
    // does across requests) must still work normally.
    let follow_up = DaemonRequest {
        id: 91,
        method: "system.status".into(),
        params: serde_json::Value::Null,
    };
    let resp = dispatch(&follow_up, &mut state, &ctx).await;
    assert_eq!(resp.id, 91);
    assert!(
        resp.error.is_none(),
        "dispatch must keep working after PayloadTooLarge: {resp:?}"
    );
}

#[test]
fn map_store_error_maps_each_variant() {
    use store::StoreError;

    assert!(matches!(
        super::map_store_error(StoreError::RecordNotFound {
            collection: "plans",
            id: "pl-x".into()
        }),
        RpcError::NotFound(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::AlreadyExists {
            collection: "works",
            id: "wk-x".into()
        }),
        RpcError::InvalidRequest(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::Io("disk".into())),
        RpcError::Internal(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::Closed),
        RpcError::Internal(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::VersionMismatch { found: 2, expected: 1 }),
        RpcError::Internal(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::UnparseableVersion { raw: "nope".into() }),
        RpcError::Internal(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::Serde("json".into())),
        RpcError::Internal(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::Stale { expected: 1, actual: 2 }),
        RpcError::InvalidRequest(_)
    ));
    assert!(matches!(
        super::map_store_error(StoreError::DuplicateTick {
            tick_id: domain::TickId::new(),
            plan_id: domain::PlanId::new(),
            bundles: vec![domain::BundleId::new()],
        }),
        RpcError::InvalidRequest(_)
    ));
}

/// Phase A: `wake_director` fires `notify_one` on the matching Plan's
/// registered `Notify` (so a Director blocked on its idle sleep wakes
/// immediately) and is a benign no-op for an unregistered Plan.
#[tokio::test]
async fn wake_director_notifies_registered_plan_and_noops_for_absent() {
    let (_td, ctx) = stub_ctx().await;
    let plan_id = domain::PlanId::new();
    let notify = Arc::new(tokio::sync::Notify::new());
    ctx.operator_notifies
        .write()
        .await
        .insert(plan_id.clone(), notify.clone());

    // A waiter that resolves when notified. notify_one stores a permit even
    // if the waiter has not yet polled, so there is no register/wake race.
    let waiter = tokio::spawn(async move { notify.notified().await });

    ctx.wake_director(&plan_id).await;

    tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("wake_director should wake the registered Notify within 2s")
        .expect("waiter task joined");

    // Absent Plan: must not panic (benign miss, same as handle_director_chat).
    ctx.wake_director(&domain::PlanId::new()).await;
}

// --- F4: re-mint helper remaps ids + dependency edges ---

#[test]
fn remint_work_batch_remaps_ids_and_dependency_edges() {
    let plan_id = domain::PlanId::new();
    let a = domain::Work::new(plan_id.clone(), "a".to_string());
    let mut b = domain::Work::new(plan_id.clone(), "b".to_string());
    let mut c = domain::Work::new(plan_id.clone(), "c".to_string());
    // b depends on a; c depends on a and b.
    b.dependencies = vec![a.id.clone()];
    c.dependencies = vec![a.id.clone(), b.id.clone()];
    let (a_old, b_old, c_old) = (a.id.clone(), b.id.clone(), c.id.clone());

    let reminted = super::remint_work_batch(vec![a, b, c]);
    assert_eq!(reminted.len(), 3);
    let (na, nb, nc) = (reminted[0].id.clone(), reminted[1].id.clone(), reminted[2].id.clone());

    // Every id is fresh.
    assert_ne!(na, a_old);
    assert_ne!(nb, b_old);
    assert_ne!(nc, c_old);

    // Dependency edges are remapped to the new sibling ids, not stale.
    assert_eq!(reminted[1].dependencies, vec![na.clone()], "b's dep -> a's new id");
    assert_eq!(
        reminted[2].dependencies,
        vec![na, nb],
        "c's deps -> a's and b's new ids"
    );
}
