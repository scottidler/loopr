//! Controlled failure-path integration tests (gate-hardening Phase E).
//!
//! These drive the real daemon in-process (`spawn_test_daemon`) with a
//! scripted LLM and poll the on-disk JSONL store for terminal state, the
//! same idiom as `stage_8_plan_to_tick.rs` — but they exercise the
//! recovery and multi-Work paths the single passing e2e never did:
//!
//! - Scenario 2: reject -> recover -> success (one Work, FIFO).
//! - Scenario 1: multi-Work DAG with dependency unblock (keyed stub).
//! - Scenario 3: mid-DAG failure + recovery + downstream unblock (keyed).
//!
//! The Director is wrapped in `FailureDirectorAwareLlm`, which inspects
//! its state-summary prompt and emits `accept_bundle` for a Reviewed
//! Bundle or `override_work {Ready}` for a Blocked Work — the recovery
//! action the daemon needs to re-drive a rejected Work. Implementer /
//! Reviewer calls (model = None) forward to the inner `ScriptedLlm`.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;

use domain::{Bundle, BundleStatus, PlanId, Tick, Work, WorkStatus};
use ipc::{MethodName, PROTOCOL_VERSION, PlanCreateResult};
use llm::{LlmClient, LlmError, Message, MessageContent, MessageRole, ScriptedLlm, ToolCall, ToolSchema, Usage};
use loopr::transport::IpcClient;

use common::harness::spawn_test_daemon;

// ---------------------------------------------------------------------------
// Scenario 2: reject -> recover -> success.
// ---------------------------------------------------------------------------

/// Decomposer: one Work, no deps.
fn one_work_decomposition() -> ToolCall {
    ToolCall {
        tool_name: "submit_decomposition".to_string(),
        input: json!({
            "children": [
                {
                    "title": "Add VERSION.md",
                    "content": "Create a top-level VERSION.md file.",
                    "dependencies": [],
                    "acceptance_criteria": ["VERSION.md exists at repo root"],
                }
            ]
        }),
    }
}

fn write_version_bundle() -> String {
    r#"[
      {"type":"run_tool","tool":"write","input":{"path":"VERSION.md","content":"v1\n"}},
      {"type":"propose_bundle","claims":["added VERSION.md"]}
    ]"#
    .to_string()
}

const REVIEWER_REJECT: &str = r#"{"kind":"reject","reason":"first attempt rejected on purpose"}"#;
const REVIEWER_ACCEPT: &str = r#"{"kind":"accept","summary":"VERSION.md present"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reject_then_recover_reaches_tick() {
    let stub = ScriptedLlm::new();
    // FIFO order matches the causal flow: decompose, impl#1, reject,
    // impl#2 (retry), accept. One Work means dispatch is unambiguous.
    stub.queue_tool(Ok(one_work_decomposition()));
    stub.queue_free(Ok(write_version_bundle())); // attempt 1
    stub.queue_free(Ok(REVIEWER_REJECT.to_string())); // reviewer rejects
    stub.queue_free(Ok(write_version_bundle())); // attempt 2 (after override)
    stub.queue_free(Ok(REVIEWER_ACCEPT.to_string())); // reviewer accepts

    let llm = FailureDirectorAwareLlm::new(stub.clone());
    let mut daemon = spawn_test_daemon(llm).await.expect("spawn_test_daemon");
    let target = daemon.target().to_path_buf();

    let plan_id = create_plan(&daemon, "Add a VERSION.md file to the repo root").await;

    // Recovery scenario: Blocked is transient (the Director re-drives), so
    // wait WITHOUT fast-failing on Blocked.
    let deadline = Duration::from_secs(30);
    tokio::select! {
        r = wait_for_tick(&target, &plan_id, deadline) => r.expect("pipeline reached Tick before deadline"),
        join = daemon.task_mut() => {
            let msg = match join {
                Ok(Ok(_)) => "daemon returned Ok before the test asserted".to_string(),
                Ok(Err(e)) => format!("daemon returned LooprError: {e}"),
                Err(join_err) => format!("daemon task join error (panic?): {join_err}"),
            };
            panic!("daemon task completed early: {msg}");
        }
    }

    let works = load_works(&target, &plan_id);
    assert_eq!(works.len(), 1, "expected exactly one Work");
    assert_eq!(works[0].status, WorkStatus::Done, "Work should recover to Done");
    assert!(
        works[0].attempt_count >= 2,
        "Work should have been re-driven at least once (attempt_count >= 2), got {}",
        works[0].attempt_count
    );

    let bundles = load_bundles(&target, works[0].id.as_ref());
    assert_eq!(bundles.len(), 2, "expected two Bundles (rejected + merged)");
    assert!(
        bundles.iter().any(|b| b.status == BundleStatus::Rejected),
        "first Bundle should be Rejected"
    );
    assert!(
        bundles.iter().any(|b| b.status == BundleStatus::Merged),
        "recovery Bundle should be Merged"
    );

    assert_eq!(load_ticks(&target, &plan_id).len(), 1, "expected one Tick");

    daemon.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Scenario 1: multi-Work DAG with dependency unblock (keyed stub).
// ---------------------------------------------------------------------------

/// Decomposer: Work A (no deps) and Work B (depends on A by title).
fn two_work_decomposition() -> ToolCall {
    ToolCall {
        tool_name: "submit_decomposition".to_string(),
        input: json!({
            "children": [
                {
                    "title": "Add A.md",
                    "content": "Create A.md at repo root.",
                    "dependencies": [],
                    "acceptance_criteria": ["A.md exists at repo root"],
                },
                {
                    "title": "Add B.md",
                    "content": "Create B.md at repo root.",
                    "dependencies": ["Add A.md"],
                    "acceptance_criteria": ["B.md exists at repo root"],
                }
            ]
        }),
    }
}

/// Implementer response writing `<name>` and proposing a bundle. The claim
/// embeds the filename so the Reviewer prompt — which echoes the claims —
/// keys to the right Work too.
fn write_named_bundle(name: &str) -> String {
    format!(
        r#"[
      {{"type":"run_tool","tool":"write","input":{{"path":"{name}","content":"x\n"}}}},
      {{"type":"propose_bundle","claims":["added {name}"]}}
    ]"#
    )
}

fn accept_for(name: &str) -> String {
    format!(r#"{{"kind":"accept","summary":"{name} present at repo root"}}"#)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_work_dag_unblocks_and_completes() {
    let stub = ScriptedLlm::new();
    stub.queue_tool(Ok(two_work_decomposition()));
    // Keyed by filename so each response binds to the right Work
    // regardless of dispatch order. Implementer is queued before Reviewer
    // for the same key, and first-match-wins by insertion order, so the
    // (causally-earlier) Implementer call consumes the Implementer entry.
    stub.queue_free_keyed("A.md", Ok(write_named_bundle("A.md")));
    stub.queue_free_keyed("A.md", Ok(accept_for("A.md")));
    stub.queue_free_keyed("B.md", Ok(write_named_bundle("B.md")));
    stub.queue_free_keyed("B.md", Ok(accept_for("B.md")));

    let llm = FailureDirectorAwareLlm::new(stub.clone());
    let mut daemon = spawn_test_daemon(llm).await.expect("spawn_test_daemon");
    let target = daemon.target().to_path_buf();

    let plan_id = create_plan(&daemon, "Add A.md then B.md (B depends on A)").await;

    let deadline = Duration::from_secs(30);
    tokio::select! {
        r = wait_for_tick(&target, &plan_id, deadline) => r.expect("both Works reached Done + Tick"),
        join = daemon.task_mut() => {
            let msg = match join {
                Ok(Ok(_)) => "daemon returned Ok before the test asserted".to_string(),
                Ok(Err(e)) => format!("daemon returned LooprError: {e}"),
                Err(join_err) => format!("daemon task join error (panic?): {join_err}"),
            };
            panic!("daemon task completed early: {msg}");
        }
    }

    let works = load_works(&target, &plan_id);
    assert_eq!(works.len(), 2, "expected two Works");
    assert!(
        works.iter().all(|w| w.status == WorkStatus::Done),
        "both Works should be Done"
    );

    // The dependent Work (B) must depend on A and both must be Merged.
    for w in &works {
        let bundles = load_bundles(&target, w.id.as_ref());
        assert_eq!(bundles.len(), 1, "each Work should have exactly one Bundle");
        assert_eq!(bundles[0].status, BundleStatus::Merged, "Bundle should be Merged");
    }
    assert!(!load_ticks(&target, &plan_id).is_empty(), "expected at least one Tick");

    daemon.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Scenario 3: mid-DAG failure + recovery + downstream unblock (keyed).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_dag_failure_recovers_then_unblocks_downstream() {
    let stub = ScriptedLlm::new();
    stub.queue_tool(Ok(two_work_decomposition()));
    // Work A: first attempt rejected, second accepted. Work B: accepted.
    // All keyed by filename; within a key, insertion order is the causal
    // order (impl#1, reject, impl#2, accept for A; impl, accept for B).
    stub.queue_free_keyed("A.md", Ok(write_named_bundle("A.md"))); // A attempt 1
    stub.queue_free_keyed("A.md", Ok(REVIEWER_REJECT.to_string())); // reject A#1
    stub.queue_free_keyed("A.md", Ok(write_named_bundle("A.md"))); // A attempt 2 (after override)
    stub.queue_free_keyed("A.md", Ok(accept_for("A.md"))); // accept A#2
    stub.queue_free_keyed("B.md", Ok(write_named_bundle("B.md"))); // B
    stub.queue_free_keyed("B.md", Ok(accept_for("B.md"))); // accept B

    let llm = FailureDirectorAwareLlm::new(stub.clone());
    let mut daemon = spawn_test_daemon(llm).await.expect("spawn_test_daemon");
    let target = daemon.target().to_path_buf();

    let plan_id = create_plan(&daemon, "Add A.md (will retry) then B.md").await;

    let deadline = Duration::from_secs(40);
    tokio::select! {
        r = wait_for_tick(&target, &plan_id, deadline) => r.expect("both Works reached Done + Tick after recovery"),
        join = daemon.task_mut() => {
            let msg = match join {
                Ok(Ok(_)) => "daemon returned Ok before the test asserted".to_string(),
                Ok(Err(e)) => format!("daemon returned LooprError: {e}"),
                Err(join_err) => format!("daemon task join error (panic?): {join_err}"),
            };
            panic!("daemon task completed early: {msg}");
        }
    }

    let works = load_works(&target, &plan_id);
    assert_eq!(works.len(), 2, "expected two Works");
    assert!(
        works.iter().all(|w| w.status == WorkStatus::Done),
        "both Works should be Done"
    );

    // The Work that was rejected (A) carries two Bundles (Rejected + Merged)
    // and attempt_count >= 2; the downstream Work (B) carries one Merged.
    let mut saw_recovered = false;
    let mut saw_clean = false;
    for w in &works {
        let bundles = load_bundles(&target, w.id.as_ref());
        if bundles.len() == 2 {
            saw_recovered = true;
            assert!(bundles.iter().any(|b| b.status == BundleStatus::Rejected));
            assert!(bundles.iter().any(|b| b.status == BundleStatus::Merged));
            assert!(w.attempt_count >= 2, "recovered Work attempt_count >= 2");
        } else {
            saw_clean = true;
            assert_eq!(bundles.len(), 1);
            assert_eq!(bundles[0].status, BundleStatus::Merged);
        }
    }
    assert!(saw_recovered, "one Work should have recovered from a rejection");
    assert!(saw_clean, "the downstream Work should have a single clean Bundle");
    assert!(!load_ticks(&target, &plan_id).is_empty(), "expected at least one Tick");

    daemon.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Shared client + Director-aware LLM wrapper.
// ---------------------------------------------------------------------------

/// Connect, handshake, drive `plan.create`, return the new Plan id.
async fn create_plan<L: LlmClient + Send + Sync + 'static>(
    daemon: &common::harness::TestDaemon<L>,
    goal: &str,
) -> PlanId {
    let mut client = IpcClient::connect(
        daemon.handle().socket_path(),
        loopr::transport::ClientTimeouts::default(),
    )
    .await
    .expect("connect");
    let hs = client.handshake(None).await.expect("handshake");
    assert_eq!(hs.protocol_version, PROTOCOL_VERSION);
    let (resp, _events) = client
        .request(MethodName::PlanCreate, json!({ "goal": goal }))
        .await
        .expect("plan.create request");
    assert!(resp.error.is_none(), "plan.create errored: {:?}", resp.error);
    let result: PlanCreateResult = serde_json::from_value(resp.result.expect("result")).expect("decode");
    result.plan.id
}

/// Wraps a `ScriptedLlm` so Director calls (model =
/// `Some("claude-opus-4-7")`) emit a recovery-aware action: `accept_bundle`
/// for a Reviewed Bundle, else `override_work {Ready}` for a Blocked Work,
/// else `done`. Implementer / Reviewer / decomposer calls forward to the
/// inner stub (which applies prompt-content keying then model-FIFO).
#[derive(Clone)]
struct FailureDirectorAwareLlm {
    inner: ScriptedLlm,
}

impl FailureDirectorAwareLlm {
    fn new(inner: ScriptedLlm) -> Self {
        Self { inner }
    }
}

/// First token starting with `prefix` on any line that contains `status`.
fn extract_id_on_status_line(text: &str, status: &str, prefix: &str) -> Option<String> {
    for line in text.lines() {
        if !line.contains(status) {
            continue;
        }
        for token in line.split(|c: char| !c.is_alphanumeric() && c != '-') {
            if token.starts_with(prefix) && token.len() > prefix.len() {
                return Some(token.to_string());
            }
        }
    }
    None
}

impl LlmClient for FailureDirectorAwareLlm {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        self.inner.complete_with_tool(system, user, tool, model).await
    }

    async fn complete_free(
        &self,
        system: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        if model == Some("claude-opus-4-7") {
            let last_user = messages.iter().rev().find(|m| matches!(m.role, MessageRole::User));
            let user_text = last_user
                .and_then(|m| {
                    m.content.iter().find_map(|c| match c {
                        MessageContent::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            // Make progress on a Reviewed Bundle first; otherwise re-drive
            // a Blocked Work; otherwise idle.
            let body = if let Some(bid) = extract_id_on_status_line(&user_text, "Reviewed", "bd-") {
                format!(r#"[{{"action":"accept_bundle","bundle_id":"{bid}"}}]"#)
            } else if let Some(wid) = extract_id_on_status_line(&user_text, "Blocked", "wk-") {
                format!(
                    r#"[{{"action":"override_work","work_id":"{wid}","target_status":"Ready","reason":"retry after rejection"}}]"#
                )
            } else {
                r#"[{"action":"done","summary":"nothing actionable"}]"#.to_string()
            };
            return Ok((body, Usage::default()));
        }
        self.inner.complete_free(system, messages, model).await
    }
}

// ---------------------------------------------------------------------------
// On-disk JSONL polling helpers (self-contained; mirror stage_8's readers).
// ---------------------------------------------------------------------------

/// Poll until Work.status == Done AND >= 1 Tick exists, or deadline.
/// Does NOT fast-fail on Blocked — recovery scenarios pass through it.
async fn wait_for_tick(target: &Path, plan_id: &PlanId, deadline: Duration) -> Result<(), String> {
    let start = Instant::now();
    while Instant::now().duration_since(start) < deadline {
        let works = load_works(target, plan_id);
        let all_done = !works.is_empty() && works.iter().all(|w| w.status == WorkStatus::Done);
        if all_done && !load_ticks(target, plan_id).is_empty() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err("deadline exceeded before all Works reached Done + a Tick was written".to_string())
}

fn load_works(target: &Path, plan_id: &PlanId) -> Vec<Work> {
    read_jsonl::<Work>(&target.join(".loopr").join("taskstore").join("works.jsonl"))
        .into_iter()
        .filter(|w| &w.parent_id == plan_id)
        .collect()
}

fn load_bundles(target: &Path, work_id: &str) -> Vec<Bundle> {
    read_jsonl::<Bundle>(&target.join(".loopr").join("taskstore").join("bundles.jsonl"))
        .into_iter()
        .filter(|b| b.work_id.as_ref() == work_id)
        .collect()
}

fn load_ticks(target: &Path, plan_id: &PlanId) -> Vec<Tick> {
    read_jsonl::<Tick>(&target.join(".loopr").join("taskstore").join("ticks.jsonl"))
        .into_iter()
        .filter(|t| &t.plan_id == plan_id)
        .collect()
}

fn read_jsonl<T: serde::de::DeserializeOwned + HasId>(path: &Path) -> Vec<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut latest: std::collections::HashMap<String, T> = std::collections::HashMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<T>(line) {
            latest.insert(record.record_id(), record);
        }
    }
    latest.into_values().collect()
}

trait HasId {
    fn record_id(&self) -> String;
}
impl HasId for Work {
    fn record_id(&self) -> String {
        self.id.as_ref().to_string()
    }
}
impl HasId for Bundle {
    fn record_id(&self) -> String {
        self.id.as_ref().to_string()
    }
}
impl HasId for Tick {
    fn record_id(&self) -> String {
        self.id.as_ref().to_string()
    }
}
