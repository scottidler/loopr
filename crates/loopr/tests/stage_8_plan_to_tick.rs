//! Stage 8 exit criterion: full Plan → Tick pipeline with stubbed LLMs
//! on a real-git tempdir.
//!
//! Scripts a decomposer response (one Work), an implementer response
//! (one `write` tool call + `propose_bundle` in one iteration), and a
//! reviewer `Accept` verdict. Drives `plan.create` over the real IPC,
//! polls the on-disk JSONL store for terminal state (Work.Done +
//! Bundle.Merged + Tick row), and asserts. Races the poll against the
//! daemon task so any panic inside a spawned agent task surfaces as a
//! JoinError instead of a silent timeout.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;

use domain::{Bundle, BundleStatus, PlanId, Tick, Work, WorkStatus};
use ipc::{MethodName, PROTOCOL_VERSION, PlanCreateResult};
use llm::{ScriptedLlm, ToolCall};
use loopr::transport::IpcClient;

use common::harness::{TestDaemon, spawn_test_daemon};

/// Decomposer tool call: one Work, top-level VERSION.md, no deps.
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
                    "files": ["VERSION.md"],
                }
            ]
        }),
    }
}

/// Implementer ralph-loop response: write VERSION.md then propose_bundle
/// in a single iteration. The dispatcher's `propose_bundle` auto-stages
/// and commits the write before returning BundleCreated.
const IMPLEMENTER_RESPONSE: &str = r#"[
  {"type":"run_tool","tool":"write","input":{"path":"VERSION.md","content":"v1\n"}},
  {"type":"propose_bundle","claims":["added VERSION.md"]}
]"#;

/// Reviewer verdict: Accept with a one-line summary.
const REVIEWER_RESPONSE: &str = r#"{"kind":"accept","summary":"VERSION.md created at repo root"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_to_tick_happy_path() {
    let stub = ScriptedLlm::new();
    stub.queue_tool(Ok(decomposer_response()));
    stub.queue_free(Ok(IMPLEMENTER_RESPONSE.to_string()));
    stub.queue_free(Ok(REVIEWER_RESPONSE.to_string()));

    // Director's per-Plan supervisor reads the assembled state summary,
    // observes Bundle status, and emits `accept_bundle` when it sees a
    // Reviewed Bundle. The Bundle id is unknown until the Implementer
    // creates it at runtime, so we wrap the ScriptedLlm in a smart
    // dispatcher that inspects the Director's prompt and emits the right
    // action. Non-Director call sites (Implementer / Reviewer) pass
    // through to the underlying ScriptedLlm queue.
    let llm = Stage8DirectorAwareLlm::new(stub.clone());

    let probe = stub.clone();
    let mut daemon = spawn_test_daemon(llm).await.expect("spawn_test_daemon");
    let target = daemon.target().to_path_buf();

    // Drive plan.create over real IPC.
    let mut client = IpcClient::connect(
        daemon.handle().socket_path(),
        loopr::transport::ClientTimeouts::default(),
    )
    .await
    .expect("connect");
    let hs = client.handshake(None).await.expect("handshake");
    assert_eq!(hs.protocol_version, PROTOCOL_VERSION);

    let (resp, _events) = client
        .request(
            MethodName::PlanCreate,
            json!({
                "goal": "Add a VERSION.md file to the repo root"
            }),
        )
        .await
        .expect("plan.create request");
    assert!(resp.error.is_none(), "plan.create errored: {:?}", resp.error);
    let plan_create_result: PlanCreateResult =
        serde_json::from_value(resp.result.expect("result")).expect("decode PlanCreateResult");
    let plan_id = plan_create_result.plan.id.clone();

    drop(client);

    // Race three futures: success, fast-fail on Blocked, daemon task panic.
    let deadline = Duration::from_secs(20);
    let success = wait_for_tick(&target, &plan_id, deadline);
    tokio::select! {
        r = success => r.expect("pipeline reached terminal success before deadline"),
        join = daemon.task_mut() => {
            let err_msg = match join {
                Ok(Ok(_)) => "daemon returned Ok(Ok(_)) before the test asserted".to_string(),
                Ok(Err(e)) => format!("daemon returned LooprError: {e}"),
                Err(join_err) => format!("daemon task join error (panic?): {join_err}"),
            };
            panic!("daemon task completed early: {err_msg}");
        }
    }

    // Final snapshot assertions.
    let works = load_works(&target, &plan_id);
    assert_eq!(works.len(), 1, "expected exactly one Work for the Plan");
    assert_eq!(works[0].status, WorkStatus::Done);

    let bundles = load_bundles(&target, works[0].id.as_ref());
    assert_eq!(bundles.len(), 1, "expected exactly one Bundle for the Work");
    assert_eq!(bundles[0].status, BundleStatus::Merged);

    let ticks = load_ticks(&target, &plan_id);
    assert_eq!(ticks.len(), 1, "expected exactly one Tick for the Plan");

    daemon.shutdown().await.expect("shutdown");

    // Scripted queue should be fully drained — 1 tool call + 2 free calls.
    assert!(
        probe.is_empty(),
        "scripted responses left unused: {:?}",
        probe.remaining()
    );
}

/// Poll the on-disk JSONL store until we see either:
/// - success: Work.status == Done AND >= 1 Tick row for the Plan
/// - fast-fail: Work.status == Blocked (surface a pointer to the run log)
/// - deadline: return error
///
/// Reads JSONL files directly — no second Store handle to avoid contending
/// on the daemon's live AsyncStore.
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
                    let runs = target.join(".loopr").join("runs");
                    return Err(format!(
                        "Work reached Blocked; see {}/*/events.log for the reason",
                        runs.display()
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

/// Read a JSONL file, parsing the LATEST snapshot per record. taskstore
/// appends new snapshots of the same record as new lines, so the last
/// entry for a given id wins. We just keep the last line for every id.
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

// Keep TestDaemon type visible so task_mut's lifetime annotations compile.
#[allow(dead_code)]
fn _type_sink(_: &TestDaemon<Stage8DirectorAwareLlm>) {}

// ---------------------------------------------------------------------------
// Stage8DirectorAwareLlm: smart wrapper that routes Director responses by
// inspecting the prompt's Bundle table for a Reviewed entry.
// ---------------------------------------------------------------------------

use llm::{LlmClient, LlmError, Message, MessageContent, MessageRole, ToolSchema, Usage};

/// Wraps a `ScriptedLlm` so that Director-routed `complete_free` calls
/// (model = `Some("claude-opus-4-7")`) emit `accept_bundle` once the
/// prompt's Bundle table contains a `Reviewed` row, and `done` otherwise.
/// Implementer / Reviewer / decomposer calls forward to the inner stub.
#[derive(Clone)]
struct Stage8DirectorAwareLlm {
    inner: ScriptedLlm,
}

impl Stage8DirectorAwareLlm {
    fn new(inner: ScriptedLlm) -> Self {
        Self { inner }
    }
}

/// Pull the first Reviewed Bundle id out of the Director's user prompt.
/// The Director's `user.pmt` renders one line per Bundle (`id, work_id,
/// status`); we grep for `Reviewed` and the colocated `bd-…` token.
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

impl LlmClient for Stage8DirectorAwareLlm {
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
            // Inspect the most recent user turn for a Reviewed bundle id.
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
