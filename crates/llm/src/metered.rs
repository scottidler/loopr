//! `MeteredLlmClient<L>`: wraps any `LlmClient` and accumulates the
//! per-call `Usage` into a shared `ProcessSnapshot` counter struct.
//!
//! Phase 7 of the Tier-1 cleanup. The trait widening from Phase 4
//! already returns `Usage` from every call; this wrapper destructures
//! the tuple, calls `snapshot.record_llm_call(...)` under the
//! `Mutex`, then forwards the payload. The wrapper is the sole
//! counter — call sites continue to use `let (x, _usage) = ...`.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use telemetry::digest::cost::{cost_micros, rate_for};
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
    /// Unknown model ids already logged by `append`. De-dup so a long
    /// run against an unrecognized model warns once per distinct id,
    /// not once per call. Phase 4 of the verified-swarm doc — the
    /// costs.jsonl path gets its own dedup independent of
    /// `ProcessSnapshot::record_llm_call`'s (the two are different
    /// consumers of the same `Usage`, not a shared call site).
    warned_unknown_models: Mutex<HashSet<String>>,
}

impl CostSink {
    /// Build a sink writing `<loopr_dir>/costs.jsonl`. `run_id` is the
    /// daemon's process id (the `Loopr-Run` correlation key).
    pub fn new(loopr_dir: &Path, run_id: impl Into<String>) -> Self {
        Self {
            path: loopr_dir.join(COSTS_FILENAME),
            run_id: run_id.into(),
            warned_unknown_models: Mutex::new(HashSet::new()),
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
        // Phase 4: warn once per distinct unknown model id, not once
        // per call — a long run against an unrecognized model must not
        // spam costs.jsonl's warnings, but must not go silent forever
        // either. Lock scope is a single `HashSet::insert`, no `.await`
        // held across it. Mutex poison (mirrors `record`'s handling
        // below) fails open — still warn, since we can't tell whether
        // this id was warned before.
        let should_warn = rate_for(model).is_none()
            && match self.warned_unknown_models.lock() {
                Ok(mut warned) => warned.insert(model.to_string()),
                Err(_) => {
                    tracing::warn!("costs.jsonl: warned_unknown_models Mutex poisoned; warn-dedup disabled");
                    true
                }
            };
        if should_warn {
            tracing::warn!(
                model,
                "costs.jsonl: unknown model id; pricing this (and every future) call of this model as $0"
            );
        }
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
/// Prices by the call's OWN `usage.model` (Phase 4 of the
/// verified-swarm doc) — `"unknown"` when the inner client didn't set
/// one (stub/test responses), never the snapshot's configured/primary
/// model. Mutex poison is non-fatal: a poisoned lock means a previous
/// holder panicked, and the digest is already going to render
/// `abnormal_exit` — the metering counters are tolerable to lose. Emit
/// `tracing::warn!` and return.
fn record(snapshot: &Arc<Mutex<ProcessSnapshot>>, usage: &Usage) {
    let model = usage.model.as_deref().unwrap_or("unknown");
    match snapshot.lock() {
        Ok(mut snap) => snap.record_llm_call(
            model,
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

/// Phase 4 (Cost/budget attribution) regression tests. Deliberately
/// NOT gated behind `feature = "stub"` — unlike the module above, none
/// of these need `ScriptedLlm`; they call `record()` and
/// `CostSink::append()` directly. This crate's `otto ci` runs plain
/// `cargo test -p llm` (no `--features stub`), so the stub-gated
/// module above never actually executes under that gate; keeping
/// these tests feature-free is what makes them the ones that actually
/// verify this phase's success criteria under the required CI command.
#[cfg(test)]
mod pricing_tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::{CostSink, ProcessSnapshot, record};
    use crate::usage::Usage;

    fn usage_for(model: &str, input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            model: Some(model.to_string()),
        }
    }

    /// Break-to-prove regression: before Phase 4, `record()` called
    /// `snapshot.record_llm_call` with the snapshot's own counts and no
    /// per-call model, so every call priced at whatever model the
    /// snapshot was constructed with. A snapshot built for Sonnet but
    /// fed an Opus `Usage` used to price at the Sonnet rate; this test
    /// fails on that behavior and passes once pricing follows
    /// `usage.model`.
    #[test]
    fn record_prices_by_usage_model_not_snapshot_model() {
        let snapshot = Arc::new(Mutex::new(ProcessSnapshot::new("claude-sonnet-4-6")));
        record(&snapshot, &usage_for("claude-opus-4-7", 1_000_000, 1_000_000));
        let snap = snapshot.lock().unwrap();
        let expected_opus = telemetry::digest::cost::cost_micros("claude-opus-4-7", 1_000_000, 1_000_000, 0, 0);
        let wrong_sonnet = telemetry::digest::cost::cost_micros("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0);
        assert_eq!(snap.llm_cost_micros, expected_opus);
        assert_ne!(snap.llm_cost_micros, wrong_sonnet);
    }

    /// Two-model run: each call through the free `record()` fn prices
    /// independently by its own `usage.model`.
    #[tokio::test]
    async fn cost_sink_append_prices_two_models_independently() {
        let dir = tempfile::TempDir::new().unwrap();
        let sink = CostSink::new(dir.path(), "pc-two-model");
        sink.append(&usage_for("claude-sonnet-4-6", 1_000_000, 1_000_000)).await;
        sink.append(&usage_for("claude-opus-4-7", 1_000_000, 1_000_000)).await;

        let body = std::fs::read_to_string(dir.path().join("costs.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        let sonnet_line = lines.iter().find(|v| v["model"] == "claude-sonnet-4-6").unwrap();
        let opus_line = lines.iter().find(|v| v["model"] == "claude-opus-4-7").unwrap();
        let expected_sonnet =
            telemetry::digest::cost::cost_micros("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0) as f64 / 1_000_000.0;
        let expected_opus =
            telemetry::digest::cost::cost_micros("claude-opus-4-7", 1_000_000, 1_000_000, 0, 0) as f64 / 1_000_000.0;
        assert!((sonnet_line["cost_usd"].as_f64().unwrap() - expected_sonnet).abs() < f64::EPSILON);
        assert!((opus_line["cost_usd"].as_f64().unwrap() - expected_opus).abs() < f64::EPSILON);
        assert!(
            (sonnet_line["cost_usd"].as_f64().unwrap() - opus_line["cost_usd"].as_f64().unwrap()).abs() > f64::EPSILON,
            "the two models must price differently"
        );
    }

    /// Small event-capture layer: records the rendered `message` field
    /// of every WARN-level event. Mirrors the JSON-capture pattern used
    /// by `telemetry::tests` (`compose_emits_json_event_with_expected_fields`),
    /// scoped down to just level+message since that's all this test needs.
    #[derive(Clone, Default)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    impl VecWriter {
        fn snapshot(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl std::io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Count JSON log lines at WARN level whose message contains `needle`.
    fn count_warn_lines(json: &str, needle: &str) -> usize {
        json.lines()
            .filter(|line| {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    return false;
                };
                v.get("level").and_then(|l| l.as_str()) == Some("WARN")
                    && v.get("fields")
                        .and_then(|f| f.get("message"))
                        .and_then(|m| m.as_str())
                        .map(|m| m.contains(needle))
                        .unwrap_or(false)
            })
            .count()
    }

    /// The costs.jsonl path's own warn-dedup: three calls against the
    /// same unknown model produce exactly one WARN, and every call
    /// still lands a nonzero-usage ledger row (only the price is $0).
    #[tokio::test]
    async fn cost_sink_append_unknown_model_warns_once_and_still_ledgers_usage() {
        let writer = VecWriter::default();
        let json_layer = tracing_subscriber::fmt::layer().json().with_writer(writer.clone());
        let subscriber = tracing_subscriber::registry().with(json_layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let dir = tempfile::TempDir::new().unwrap();
        let sink = CostSink::new(dir.path(), "pc-unknown");
        for _ in 0..3 {
            sink.append(&usage_for("not-a-real-model", 10, 20)).await;
        }
        drop(_guard);

        let log = writer.snapshot();
        assert_eq!(
            count_warn_lines(&log, "unknown model id"),
            1,
            "expected exactly one WARN for the repeated unknown model id; got log: {log}"
        );

        let body = std::fs::read_to_string(dir.path().join("costs.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3, "every call still lands a ledger row");
        for line in &lines {
            assert_eq!(line["input_tokens"], 10);
            assert_eq!(line["output_tokens"], 20);
            assert_eq!(line["cost_usd"], 0.0, "unknown model prices at $0");
        }
    }
}
