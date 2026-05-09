//! Stage 9 / Director Phase 3 integration test: full Plan -> Tick pipeline
//! with the per-Plan Director driving Bundle acceptance.
//!
//! Differs from `stage_8_plan_to_tick.rs` only in framing: `stage_8`
//! tests the deterministic Implementer -> Reviewer -> Director ->
//! Integrator path; this file is the canonical Phase 3 deliverable
//! called out in `docs/design/2026-05-08-director-phase-1.md` and
//! pinned to that handoff. Asserts:
//!
//! 1. Director's `accept_bundle` action drives the Reviewed Bundle to
//!    Accepted, and the Integrator runs to completion.
//! 2. Director's reconcile sweep promotes Integrated -> Done.
//! 3. Director's GoalComplete check exits the per-Plan task once every
//!    Work is terminal with at least one Done (the Director task drains
//!    cleanly at shutdown).
//!
//! Stubbed Anthropic; real `Store`. The Director's LLM responses are
//! routed via the model-aware ScriptedLlm queue
//! (`queue_free_for("claude-opus-4-7", ...)`); Implementer / Reviewer
//! responses go through the default queue.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;

use domain::{Bundle, BundleStatus, PlanId, Tick, Work, WorkStatus};
use ipc::{MethodName, PROTOCOL_VERSION, PlanCreateResult};
use llm::{LlmClient, LlmError, Message, MessageContent, MessageRole, ScriptedLlm, ToolCall, ToolSchema, Usage};
use loopr::transport::IpcClient;

use common::harness::{TestDaemon, spawn_test_daemon};

fn decomposer_response() -> ToolCall {
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

const IMPLEMENTER_RESPONSE: &str = r#"[
  {"type":"run_tool","tool":"write","input":{"path":"VERSION.md","content":"v1\n"}},
  {"type":"propose_bundle","claims":["added VERSION.md"]}
]"#;

const REVIEWER_RESPONSE: &str = r#"{"kind":"accept","summary":"VERSION.md created at repo root"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn director_plan_to_tick_happy_path() {
    let stub = ScriptedLlm::new();
    stub.queue_tool(Ok(decomposer_response()));
    stub.queue_free(Ok(IMPLEMENTER_RESPONSE.to_string()));
    stub.queue_free(Ok(REVIEWER_RESPONSE.to_string()));

    let llm = DirectorAwareLlm::new(stub.clone());
    let probe = stub.clone();
    let mut daemon = spawn_test_daemon(llm).await.expect("spawn_test_daemon");
    let target = daemon.target().to_path_buf();

    let mut client = IpcClient::connect(daemon.handle().socket_path())
        .await
        .expect("connect");
    let hs = client.handshake(None).await.expect("handshake");
    assert_eq!(hs.protocol_version, PROTOCOL_VERSION);

    let (resp, _events) = client
        .request(
            MethodName::PlanCreate,
            json!({"goal": "Add a VERSION.md file to the repo root"}),
        )
        .await
        .expect("plan.create request");
    assert!(resp.error.is_none(), "plan.create errored: {:?}", resp.error);
    let plan_create_result: PlanCreateResult =
        serde_json::from_value(resp.result.expect("result")).expect("decode PlanCreateResult");
    let plan_id = plan_create_result.plan.id.clone();
    drop(client);

    let deadline = Duration::from_secs(20);
    let success = wait_for_tick(&target, &plan_id, deadline);
    tokio::select! {
        r = success => r.expect("pipeline reached terminal success before deadline"),
        join = daemon.task_mut() => {
            let err_msg = match join {
                Ok(Ok(_)) => "daemon returned Ok before assertion".to_string(),
                Ok(Err(e)) => format!("daemon LooprError: {e}"),
                Err(j) => format!("daemon join error: {j}"),
            };
            panic!("daemon task completed early: {err_msg}");
        }
    }

    let works = load_works(&target, &plan_id);
    assert_eq!(works.len(), 1);
    assert_eq!(
        works[0].status,
        WorkStatus::Done,
        "Director must promote Integrated -> Done"
    );

    let bundles = load_bundles(&target, works[0].id.as_ref());
    assert_eq!(bundles.len(), 1);
    assert_eq!(
        bundles[0].status,
        BundleStatus::Merged,
        "Director's accept_bundle must drive Bundle to Merged via Integrator"
    );

    let ticks = load_ticks(&target, &plan_id);
    assert_eq!(ticks.len(), 1, "Integrator must produce exactly one Tick");

    daemon.shutdown().await.expect("shutdown");

    assert!(
        probe.is_empty(),
        "scripted (non-Director) responses left unused: {:?}",
        probe.remaining()
    );
}

async fn wait_for_tick(target: &Path, plan_id: &PlanId, deadline: Duration) -> Result<(), String> {
    let start = Instant::now();
    while Instant::now().duration_since(start) < deadline {
        let works = load_works(target, plan_id);
        if let Some(w) = works.first() {
            match w.status {
                WorkStatus::Done => {
                    let ticks = load_ticks(target, plan_id);
                    if !ticks.is_empty() {
                        return Ok(());
                    }
                }
                WorkStatus::Blocked => {
                    return Err(format!(
                        "Work reached Blocked; see {}/.loopr/runs for the reason",
                        target.display()
                    ));
                }
                _ => {}
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err("deadline exceeded before Work reached Done + Tick was written".to_string())
}

fn load_works(target: &Path, plan_id: &PlanId) -> Vec<Work> {
    let path = target.join(".loopr").join("taskstore").join("works.jsonl");
    read_jsonl::<Work>(&path)
        .into_iter()
        .filter(|w| &w.parent_id == plan_id)
        .collect()
}

fn load_bundles(target: &Path, work_id: &str) -> Vec<Bundle> {
    let path = target.join(".loopr").join("taskstore").join("bundles.jsonl");
    read_jsonl::<Bundle>(&path)
        .into_iter()
        .filter(|b| b.work_id.as_ref() == work_id)
        .collect()
}

fn load_ticks(target: &Path, plan_id: &PlanId) -> Vec<Tick> {
    let path = target.join(".loopr").join("taskstore").join("ticks.jsonl");
    read_jsonl::<Tick>(&path)
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

#[allow(dead_code)]
fn _type_sink(_: &TestDaemon<DirectorAwareLlm>) {}

// ---------------------------------------------------------------------------
// DirectorAwareLlm: smart wrapper that watches the Director's prompts for
// a Reviewed Bundle and emits `accept_bundle` for that id. All other call
// sites pass through to the underlying ScriptedLlm.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DirectorAwareLlm {
    inner: ScriptedLlm,
}

impl DirectorAwareLlm {
    fn new(inner: ScriptedLlm) -> Self {
        Self { inner }
    }
}

fn extract_reviewed_bundle_id(text: &str) -> Option<String> {
    for line in text.lines() {
        if !line.contains("Reviewed") {
            continue;
        }
        for token in line.split(|c: char| !c.is_alphanumeric() && c != '-') {
            if token.starts_with("bd-") && token.len() > 3 {
                return Some(token.to_string());
            }
        }
    }
    None
}

impl LlmClient for DirectorAwareLlm {
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
            let body = if let Some(bid) = extract_reviewed_bundle_id(&user_text) {
                format!(r#"[{{"action":"accept_bundle","bundle_id":"{bid}"}}]"#)
            } else {
                r#"[{"action":"done","summary":"awaiting reviewer"}]"#.to_string()
            };
            return Ok((body, Usage::default()));
        }
        self.inner.complete_free(system, messages, model).await
    }
}
