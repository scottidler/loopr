//! `MeteredLlmClient<L>`: wraps any `LlmClient` and accumulates the
//! per-call `Usage` into a shared `ProcessSnapshot` counter struct.
//!
//! Phase 7 of the Tier-1 cleanup. The trait widening from Phase 4
//! already returns `Usage` from every call; this wrapper destructures
//! the tuple, calls `snapshot.record_llm_call(...)` under the
//! `Mutex`, then forwards the payload. The wrapper is the sole
//! counter — call sites continue to use `let (x, _usage) = ...`.

use std::sync::{Arc, Mutex};

use telemetry::digest::process::ProcessSnapshot;

use crate::client::LlmClient;
use crate::error::LlmError;
use crate::message::Message;
use crate::tool::{ToolCall, ToolSchema};
use crate::usage::Usage;

/// Wraps an inner `LlmClient` and records every call's `Usage` into a
/// shared `ProcessSnapshot`. Cheap to clone via `Arc<Mutex<_>>`.
pub struct MeteredLlmClient<L> {
    inner: L,
    snapshot: Arc<Mutex<ProcessSnapshot>>,
}

impl<L> MeteredLlmClient<L> {
    pub fn new(inner: L, snapshot: Arc<Mutex<ProcessSnapshot>>) -> Self {
        Self { inner, snapshot }
    }
}

impl<L: LlmClient + Send + Sync> LlmClient for MeteredLlmClient<L> {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
    ) -> Result<(ToolCall, Usage), LlmError> {
        let (tc, usage) = self.inner.complete_with_tool(system, user, tool).await?;
        record(&self.snapshot, &usage);
        Ok((tc, usage))
    }

    async fn complete_free(&self, system: &str, messages: &[Message]) -> Result<(String, Usage), LlmError> {
        let (raw, usage) = self.inner.complete_free(system, messages).await?;
        record(&self.snapshot, &usage);
        Ok((raw, usage))
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
        let (out, _) = metered.complete_free("s", &[]).await.unwrap();
        assert_eq!(out, "ok");
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.llm_calls, 1);
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
        let _ = metered.complete_with_tool("", "", schema).await.unwrap();
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.llm_calls, 1);
    }
}
