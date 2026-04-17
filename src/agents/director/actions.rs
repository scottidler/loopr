//! Director action vocabulary and execution.
//!
//! Shared between Escalation (Phase 5) and UserIntervention (Phase 6). The LLM emits a
//! structured JSON payload that we decode into `DirectorAction` values, then `execute`
//! dispatches each action through the `AgentIpcBridge`.
//!
//! Design doc section "Director Modes → Escalation" defines the vocabulary:
//! - `revise-work { work_id, acceptance_criteria? }`: rewrite a Work's AC and reset it to Pending
//! - `re-decompose { target_type, target_id, reason? }`: transition parent spec or phase to
//!   Draft so the engine re-runs decomposition
//! - `abandon-work { work_id, reason? }`: move a Work to Abandoned
//! - `spawn-researcher { query, scope? }`: dispatch a Researcher to investigate
//! - `message-user { text }`: emit a `director.diagnosis` event for the TUI
//!
//! The parser is tolerant: unknown action types are logged and skipped rather than failing
//! the whole batch, so a single bad action doesn't strand the Director in Escalation.

use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::agents::bridge::AgentIpcBridge;
use crate::ipc::protocol::DaemonEvent;

/// Which level of the hierarchy a `re-decompose` action targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReDecomposeTarget {
    Spec,
    Phase,
}

impl ReDecomposeTarget {
    pub fn method(self) -> &'static str {
        match self {
            ReDecomposeTarget::Spec => "spec.transition",
            ReDecomposeTarget::Phase => "phase.transition",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ReDecomposeTarget::Spec => "spec",
            ReDecomposeTarget::Phase => "phase",
        }
    }
}

/// Structured action the Director can emit from an LLM call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DirectorAction {
    ReviseWork {
        work_id: String,
        #[serde(default)]
        acceptance_criteria: Option<Vec<String>>,
        #[serde(default)]
        title: Option<String>,
    },
    ReDecompose {
        target_type: ReDecomposeTarget,
        target_id: String,
        #[serde(default)]
        reason: Option<String>,
    },
    AbandonWork {
        work_id: String,
        #[serde(default)]
        reason: Option<String>,
    },
    SpawnResearcher {
        query: String,
        #[serde(default)]
        scope: Option<String>,
    },
    MessageUser {
        text: String,
    },
}

/// Summary of action-execution outcomes, useful for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ActionExecutionReport {
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
    pub details: Vec<String>,
}

impl ActionExecutionReport {
    pub fn record_ok(&mut self, detail: impl Into<String>) {
        self.ok += 1;
        self.details.push(format!("ok: {}", detail.into()));
    }
    pub fn record_failed(&mut self, detail: impl Into<String>) {
        self.failed += 1;
        self.details.push(format!("failed: {}", detail.into()));
    }
    pub fn record_skipped(&mut self, detail: impl Into<String>) {
        self.skipped += 1;
        self.details.push(format!("skipped: {}", detail.into()));
    }
}

/// Parse an LLM response into a vector of `DirectorAction`s.
///
/// Accepts three shapes to stay resilient to LLM formatting drift:
/// 1. A top-level JSON object `{"actions": [...]}` (preferred).
/// 2. A bare JSON array `[...]`.
/// 3. A code-fenced block surrounding either of the above.
///
/// Unknown action types inside the array are logged and skipped. The function returns
/// an error only when no parsable JSON is found at all.
pub fn parse_actions(response: &str) -> Result<Vec<DirectorAction>> {
    let trimmed = response.trim();
    let candidate = extract_json_block(trimmed).unwrap_or(trimmed);

    let value: Value =
        serde_json::from_str(candidate).map_err(|e| eyre!("director response is not valid JSON: {}", e))?;

    let array = match &value {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .get("actions")
            .and_then(|v| v.as_array())
            .cloned()
            .ok_or_else(|| eyre!("director response object must have an 'actions' array"))?,
        _ => return Err(eyre!("director response must be an array or an object with 'actions'")),
    };

    let mut actions = Vec::with_capacity(array.len());
    for item in array {
        match serde_json::from_value::<DirectorAction>(item.clone()) {
            Ok(a) => actions.push(a),
            Err(e) => warn!("director: skipping unparseable action {}: {}", item, e),
        }
    }
    Ok(actions)
}

/// If `text` contains a ```json ... ``` fenced block, return its contents; otherwise None.
/// Used as a lightweight fallback so LLMs that wrap JSON in markdown don't crash the parser.
fn extract_json_block(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let rest = &text[start + 3..];
    let after_lang = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest);
    let end = after_lang.find("```")?;
    Some(after_lang[..end].trim())
}

/// Execute a batch of actions against the daemon via the IPC bridge.
///
/// Each action is dispatched independently. Failures are recorded in the report but don't
/// halt the batch - the Director prefers partial progress over aborting mid-recovery.
///
/// For every action, a `director.action_taken` event is emitted with the action type,
/// target id (if any), and a short result string. The TUI renders these in the audit trail.
pub fn execute_actions(
    actions: &[DirectorAction],
    bridge: &AgentIpcBridge,
    event_tx: &broadcast::Sender<DaemonEvent>,
    director_session_id: &str,
) -> ActionExecutionReport {
    let mut report = ActionExecutionReport::default();
    for action in actions {
        execute_one(action, bridge, event_tx, director_session_id, &mut report);
    }
    report
}

fn execute_one(
    action: &DirectorAction,
    bridge: &AgentIpcBridge,
    event_tx: &broadcast::Sender<DaemonEvent>,
    director_session_id: &str,
    report: &mut ActionExecutionReport,
) {
    let (kind, target_id, outcome) = match action {
        DirectorAction::ReviseWork {
            work_id,
            acceptance_criteria,
            title,
        } => {
            let mut params = serde_json::Map::new();
            params.insert("id".into(), Value::String(work_id.clone()));
            if let Some(ac) = acceptance_criteria {
                params.insert(
                    "acceptance_criteria".into(),
                    Value::Array(ac.iter().map(|s| Value::String(s.clone())).collect()),
                );
            }
            if let Some(t) = title {
                params.insert("title".into(), Value::String(t.clone()));
            }
            let update = bridge.request("work.update", Value::Object(params));
            if update.is_error() {
                let msg = update.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                (
                    "revise-work",
                    Some(work_id.clone()),
                    format!("work.update failed: {}", msg),
                )
            } else {
                // Reset to Pending so the engine can re-pick it up.
                let transition = bridge.request(
                    "work.transition",
                    serde_json::json!({
                        "id": work_id,
                        "target_status": "Pending",
                        "role": "director",
                        "override": true,
                        "reason": "director-revise",
                    }),
                );
                if transition.is_error() {
                    let msg = transition.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    (
                        "revise-work",
                        Some(work_id.clone()),
                        format!("updated but transition failed: {}", msg),
                    )
                } else {
                    (
                        "revise-work",
                        Some(work_id.clone()),
                        "updated + reset to Pending".into(),
                    )
                }
            }
        }
        DirectorAction::ReDecompose {
            target_type,
            target_id,
            reason,
        } => {
            let reason_str = reason.clone().unwrap_or_else(|| "director-redecompose".into());
            let resp = bridge.request(
                target_type.method(),
                serde_json::json!({
                    "id": target_id,
                    "target_status": "draft",
                    "role": "director",
                    "override": true,
                    "reason": reason_str,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                (
                    "re-decompose",
                    Some(target_id.clone()),
                    format!("{} failed: {}", target_type.method(), msg),
                )
            } else {
                (
                    "re-decompose",
                    Some(target_id.clone()),
                    format!("{} transitioned to Draft", target_type.as_str()),
                )
            }
        }
        DirectorAction::AbandonWork { work_id, reason } => {
            let resp = bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": work_id,
                    "target_status": "Abandoned",
                    "role": "director",
                    "override": true,
                    "reason": reason.clone().unwrap_or_else(|| "director-abandon".into()),
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                (
                    "abandon-work",
                    Some(work_id.clone()),
                    format!("transition failed: {}", msg),
                )
            } else {
                (
                    "abandon-work",
                    Some(work_id.clone()),
                    "transitioned to Abandoned".into(),
                )
            }
        }
        DirectorAction::SpawnResearcher { query, scope } => {
            let resp = bridge.request(
                "agent.start",
                serde_json::json!({
                    "agent_type": "researcher",
                    "query": query,
                    "target_id": scope,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                ("spawn-researcher", None, format!("agent.start failed: {}", msg))
            } else {
                let sid = resp
                    .result
                    .as_ref()
                    .and_then(|v| v.get("session_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                (
                    "spawn-researcher",
                    Some(sid.clone()),
                    format!("spawned researcher {}", sid),
                )
            }
        }
        DirectorAction::MessageUser { text } => {
            // `director.diagnosis` carries a plain text message the TUI renders into chat.
            let _ = event_tx.send(DaemonEvent::new(
                "director.diagnosis",
                serde_json::json!({
                    "session_id": director_session_id,
                    "text": text,
                }),
            ));
            ("message-user", None, "diagnosis emitted".into())
        }
    };

    let is_ok = !outcome.starts_with("failed") && !outcome.contains("failed:");
    debug!("director action {}: target={:?} outcome={}", kind, target_id, outcome);
    let _ = event_tx.send(DaemonEvent::new(
        "director.action_taken",
        serde_json::json!({
            "session_id": director_session_id,
            "action": kind,
            "target_id": target_id,
            "result": outcome,
        }),
    ));
    if is_ok {
        report.record_ok(format!("{}: {}", kind, outcome));
        info!("director action {} succeeded", kind);
    } else {
        report.record_failed(format!("{}: {}", kind, outcome));
        warn!("director action {} failed: {}", kind, outcome);
    }
}
