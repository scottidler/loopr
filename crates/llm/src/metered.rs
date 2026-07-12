//! `MeteredLlmClient<L>`: wraps any `LlmClient` and accumulates the
//! per-call `Usage` into a shared `ProcessSnapshot` counter struct.
//!
//! Phase 7 of the Tier-1 cleanup. The trait widening from Phase 4
//! already returns `Usage` from every call; this wrapper destructures
//! the tuple, calls `snapshot.record_llm_call(...)` under the
//! `Mutex`, then forwards the payload. The wrapper is the sole
//! counter — call sites continue to use `let (x, _usage) = ...`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use telemetry::digest::cost::cost_micros;
use telemetry::digest::process::ProcessSnapshot;

use crate::call::CallContext;
use crate::client::LlmClient;
use crate::error::LlmError;
use crate::message::Message;
use crate::tool::{ToolCall, ToolSchema};
use crate::usage::Usage;

/// Filename of the append-only per-call cost ledger under `.loopr/`.
const COSTS_FILENAME: &str = "costs.jsonl";

/// Append-only `.loopr/costs.jsonl` writer: one JSON line per LLM call
/// (the vision's "Cost audit" shape). Shared across the daemon's tasks;
/// each call's Plan/Work/role come from the [`CallContext`] task-local.
/// `loopr costs` is a trivial `jq` consumer of this file (no Rust CLI).
pub struct CostSink {
    path: PathBuf,
    run_id: String,
}

impl CostSink {
    /// Build a sink writing `<loopr_dir>/costs.jsonl`. `run_id` is the
    /// daemon's process id (the `Loopr-Run` correlation key).
    pub fn new(loopr_dir: &Path, run_id: impl Into<String>) -> Self {
        Self {
            path: loopr_dir.join(COSTS_FILENAME),
            run_id: run_id.into(),
        }
    }

    /// Append one cost line for a completed (or billed-but-failed) call.
    /// Best-effort: a write failure logs `warn!` and is swallowed — cost
    /// audit must never break the call path.
    ///
    /// The JSON line is built inline (cheap, and `CallContext::current()`
    /// must be read on THIS task — the task-local is not visible from the
    /// blocking pool). The blocking file open + write is moved off the async
    /// worker via `spawn_blocking`, so a slow/synced `.loopr/` disk can't
    /// stall the tokio runtime mid-LLM-call. The handle is awaited so the
    /// write completes (and its outcome is logged) before the call returns.
    async fn append(&self, usage: &Usage) {
        let cc = CallContext::current();
        let model = usage.model.as_deref().unwrap_or("unknown");
        let micros = cost_micros(
            model,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
        let cost_usd = micros as f64 / 1_000_000.0;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = serde_json::json!({
            "ts": ts,
            "run_id": self.run_id,
            "plan_id": cc.plan_id,
            "work_id": cc.work_id,
            "role": cc.role,
            "model": model,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cost_usd": cost_usd,
        })
        .to_string();
        let path = self.path.clone();
        let outcome = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
            writeln!(f, "{line}")
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(path = %self.path.display(), error = %e, "costs.jsonl append failed");
            }
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "costs.jsonl append task join failed");
            }
        }
    }
}

/// Wraps an inner `LlmClient` and records every call's `Usage` into a
/// shared `ProcessSnapshot`, and (when a `CostSink` is present) appends a
/// per-call line to `.loopr/costs.jsonl`. Cheap to clone via `Arc<_>`.
pub struct MeteredLlmClient<L> {
    inner: L,
    snapshot: Arc<Mutex<ProcessSnapshot>>,
    costs: Option<Arc<CostSink>>,
}

impl<L> MeteredLlmClient<L> {
    /// Metering into the snapshot only (no cost ledger). Used by tests
    /// and any caller without a `.loopr/` directory.
    pub fn new(inner: L, snapshot: Arc<Mutex<ProcessSnapshot>>) -> Self {
        Self {
            inner,
            snapshot,
            costs: None,
        }
    }

    /// Metering into the snapshot AND the `.loopr/costs.jsonl` ledger.
    /// The production daemon path.
    pub fn with_costs(inner: L, snapshot: Arc<Mutex<ProcessSnapshot>>, costs: Arc<CostSink>) -> Self {
        Self {
            inner,
            snapshot,
            costs: Some(costs),
        }
    }

    /// Record one call's usage into the snapshot and the cost ledger.
    /// Async because the cost-ledger append hops onto `spawn_blocking`;
    /// the snapshot record stays a synchronous `Mutex` lock (no `.await`
    /// held across it — see `record`).
    async fn meter(&self, usage: &Usage) {
        record(&self.snapshot, usage);
        if let Some(sink) = &self.costs {
            sink.append(usage).await;
        }
    }
}

impl<L: LlmClient + Send + Sync> LlmClient for MeteredLlmClient<L> {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        match self.inner.complete_with_tool(system, user, tool, model).await {
            Ok((tc, usage)) => {
                self.meter(&usage).await;
                Ok((tc, usage))
            }
            Err(e) => {
                // Error-classified 200s (max_tokens truncation, tool-
                // contract refusal) bill tokens but short-circuit the
                // success path; record their usage so the expensive
                // failures reach the cost counters (Phase 1 remediation).
                if let Some(usage) = e.billed_usage() {
                    self.meter(usage).await;
                }
                Err(e)
            }
        }
    }

    async fn complete_free(
        &self,
        system: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        match self.inner.complete_free(system, messages, model).await {
            Ok((raw, usage)) => {
                self.meter(&usage).await;
                Ok((raw, usage))
            }
            Err(e) => {
                if let Some(usage) = e.billed_usage() {
                    self.meter(usage).await;
                }
                Err(e)
            }
        }
    }

    fn model(&self) -> &str {
        self.inner.model()
    }
}

/// Lock the snapshot, record the per-call counts, drop the lock.
/// Mutex poison is non-fatal: a poisoned lock means a previous holder
/// panicked, and the digest is already going to render `abnormal_exit`
/// — the metering counters are tolerable to lose. Emit `tracing::warn!`
/// and return.
fn record(snapshot: &Arc<Mutex<ProcessSnapshot>>, usage: &Usage) {
    match snapshot.lock() {
        Ok(mut snap) => snap.record_llm_call(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        ),
        Err(_) => {
            tracing::warn!("MeteredLlmClient: snapshot Mutex poisoned; per-call counts dropped");
        }
    }
}

#[cfg(all(test, feature = "stub"))]
mod tests {
    use super::*;
    use crate::stub::ScriptedLlm;

    fn fresh_snapshot() -> Arc<Mutex<ProcessSnapshot>> {
        Arc::new(Mutex::new(ProcessSnapshot::new("claude-sonnet-4-6")))
    }

    #[tokio::test]
    async fn complete_free_records_usage() {
        let snapshot = fresh_snapshot();
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("ok".to_string()));
        let metered = MeteredLlmClient::new(stub, Arc::clone(&snapshot));
        let (out, _) = metered.complete_free("s", &[], None).await.unwrap();
        assert_eq!(out, "ok");
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.llm_calls, 1);
    }

    #[tokio::test]
    async fn error_path_records_billed_usage() {
        // Phase 1 remediation: an error-classified 200 (max_tokens
        // truncation) bills tokens but returns Err. The metered client
        // must still record the usage so the expensive failure reaches
        // the cost counters.
        use crate::error::{FatalReason, LlmError};
        use crate::usage::Usage;

        let snapshot = fresh_snapshot();
        let stub = ScriptedLlm::new();
        stub.queue_free(Err(LlmError::Fatal {
            reason: FatalReason::ContextExhausted {
                used: 8192,
                limit: 8192,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 8192,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    model: None,
                },
            },
        }));
        let metered = MeteredLlmClient::new(stub, Arc::clone(&snapshot));
        let err = metered.complete_free("s", &[], None).await.unwrap_err();
        assert!(matches!(err, LlmError::Fatal { .. }));
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.llm_calls, 1, "the failed-but-billed call must be counted");
    }

    #[tokio::test]
    async fn retryable_error_records_no_usage() {
        // A genuine transient failure (no billed body) must NOT be
        // counted — billed_usage returns None for Retryable.
        use crate::error::LlmError;

        let snapshot = fresh_snapshot();
        let stub = ScriptedLlm::new();
        stub.queue_free(Err(LlmError::Retryable {
            reason: crate::error::RetryableReason::Network {
                detail: "transient".to_string(),
            },
        }));
        let metered = MeteredLlmClient::new(stub, Arc::clone(&snapshot));
        let _ = metered.complete_free("s", &[], None).await.unwrap_err();
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.llm_calls, 0, "a retryable error bills nothing");
    }

    #[tokio::test]
    async fn cost_sink_appends_line_with_call_context() {
        use crate::call::CallContext;

        let dir = tempfile::TempDir::new().unwrap();
        let sink = Arc::new(CostSink::new(dir.path(), "pc-test01"));
        let snapshot = fresh_snapshot();
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("ok".to_string()));
        let metered = MeteredLlmClient::with_costs(stub, snapshot, Arc::clone(&sink));

        let ctx = CallContext {
            plan_id: Some("p-42".to_string()),
            work_id: Some("wk-7".to_string()),
            role: Some("implementer".to_string()),
        };
        let _ = CallContext::scope(ctx, metered.complete_free("s", &[], None))
            .await
            .unwrap();

        let body = std::fs::read_to_string(dir.path().join("costs.jsonl")).unwrap();
        let line = body.lines().next().expect("one cost line");
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["run_id"], "pc-test01");
        assert_eq!(v["plan_id"], "p-42");
        assert_eq!(v["work_id"], "wk-7");
        assert_eq!(v["role"], "implementer");
        // Stub usage is all-zero with no model: the line still records
        // the shape (model "unknown", zero cost), proving attribution
        // works independently of the provider's token counts.
        assert!(v.get("input_tokens").is_some());
        assert!(v.get("cost_usd").is_some());
    }

    #[tokio::test]
    async fn cost_sink_absent_context_writes_null_attribution() {
        let dir = tempfile::TempDir::new().unwrap();
        let sink = Arc::new(CostSink::new(dir.path(), "pc-noctx"));
        let snapshot = fresh_snapshot();
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("ok".to_string()));
        let metered = MeteredLlmClient::with_costs(stub, snapshot, sink);

        // No CallContext::scope: plan/work/role record as null, the line
        // still lands (a CLI one-shot or test path).
        let _ = metered.complete_free("s", &[], None).await.unwrap();

        let body = std::fs::read_to_string(dir.path().join("costs.jsonl")).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(v["run_id"], "pc-noctx");
        assert!(v["plan_id"].is_null());
        assert!(v["role"].is_null());
    }

    #[tokio::test]
    async fn complete_with_tool_records_usage() {
        let snapshot = fresh_snapshot();
        let stub = ScriptedLlm::new();
        let schema = ToolSchema {
            name: "t".to_string(),
            description: "".to_string(),
            input_schema: serde_json::json!({}),
        };
        stub.queue_tool(Ok(ToolCall {
            tool_name: "t".to_string(),
            input: serde_json::json!({}),
        }));
        let metered = MeteredLlmClient::new(stub, Arc::clone(&snapshot));
        let _ = metered.complete_with_tool("", "", schema, None).await.unwrap();
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.llm_calls, 1);
    }
}
