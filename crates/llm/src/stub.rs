//! Scripted `LlmClient` stub for tests.
//!
//! Centralized here so tests across `agents`, `decomposer`, and `loopr`
//! share one implementation. Gated behind the `stub` cargo feature so the
//! production `loopr` binary never links it.
//!
//! Queues live behind `Arc<Mutex<_>>` and the struct is `Clone`: a test
//! can clone the stub BEFORE handing it to a daemon or agent and retain a
//! probe for post-run assertions (e.g. assert the queue was fully drained).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::client::LlmClient;
use crate::error::LlmError;
use crate::message::Message;
use crate::tool::{ToolCall, ToolSchema};
use crate::usage::Usage;

/// Per-model free-response queues. The key `None` is the default route
/// for callers that pass `model: None` to `complete_free` (Implementer,
/// Reviewer, Decomposer); `Some("claude-opus-4-7")` etc. routes the
/// Director's per-model overrides to a separate queue. Tool-call routing
/// follows the same shape.
type ModelKey = Option<String>;
type ToolQueue = VecDeque<Result<ToolCall, LlmError>>;
type FreeQueue = VecDeque<Result<String, LlmError>>;
type ToolMap = HashMap<ModelKey, ToolQueue>;
type FreeMap = HashMap<ModelKey, FreeQueue>;

#[derive(Clone, Default)]
pub struct ScriptedLlm {
    tool_responses: Arc<Mutex<ToolMap>>,
    free_responses: Arc<Mutex<FreeMap>>,
}

impl ScriptedLlm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a tool response on the default route (model=None). Existing
    /// callers preserved without change.
    pub fn queue_tool(&self, result: Result<ToolCall, LlmError>) {
        self.queue_tool_inner(None, result);
    }

    /// Queue a tool response routed to a specific model.
    pub fn queue_tool_for(&self, model: &str, result: Result<ToolCall, LlmError>) {
        self.queue_tool_inner(Some(model.to_string()), result);
    }

    fn queue_tool_inner(&self, key: ModelKey, result: Result<ToolCall, LlmError>) {
        self.tool_responses
            .lock()
            .expect("tool_responses lock")
            .entry(key)
            .or_default()
            .push_back(result);
    }

    /// Queue a free-form response on the default route (model=None).
    pub fn queue_free(&self, result: Result<String, LlmError>) {
        self.queue_free_inner(None, result);
    }

    /// Queue a free-form response routed to a specific model. Director
    /// responses use `queue_free_for("claude-opus-4-7", ...)`.
    pub fn queue_free_for(&self, model: &str, result: Result<String, LlmError>) {
        self.queue_free_inner(Some(model.to_string()), result);
    }

    fn queue_free_inner(&self, key: ModelKey, result: Result<String, LlmError>) {
        self.free_responses
            .lock()
            .expect("free_responses lock")
            .entry(key)
            .or_default()
            .push_back(result);
    }

    pub fn is_empty(&self) -> bool {
        let (t, f) = self.remaining();
        t == 0 && f == 0
    }

    pub fn remaining(&self) -> (usize, usize) {
        let t: usize = self
            .tool_responses
            .lock()
            .expect("tool_responses lock")
            .values()
            .map(|q| q.len())
            .sum();
        let f: usize = self
            .free_responses
            .lock()
            .expect("free_responses lock")
            .values()
            .map(|q| q.len())
            .sum();
        (t, f)
    }
}

impl LlmClient for ScriptedLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        let key: ModelKey = model.map(|m| m.to_string());
        let popped = {
            let mut map = self.tool_responses.lock().expect("tool_responses lock");
            map.get_mut(&key).and_then(|q| q.pop_front())
        };
        match popped {
            Some(Ok(tc)) => Ok((tc, Usage::default())),
            Some(Err(e)) => Err(e),
            None => {
                let (t, f) = self.remaining();
                panic!(
                    "ScriptedLlm: complete_with_tool called with empty queue for model={model:?} (tool remaining: {t}, free remaining: {f})"
                );
            }
        }
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        let key: ModelKey = model.map(|m| m.to_string());
        let popped = {
            let mut map = self.free_responses.lock().expect("free_responses lock");
            map.get_mut(&key).and_then(|q| q.pop_front())
        };
        match popped {
            Some(Ok(s)) => Ok((s, Usage::default())),
            Some(Err(e)) => Err(e),
            None => {
                let (t, f) = self.remaining();
                panic!(
                    "ScriptedLlm: complete_free called with empty queue for model={model:?} (tool remaining: {t}, free remaining: {f})"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FatalReason;
    use serde_json::json;

    fn tool_call(input: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_name: "test_tool".to_string(),
            input,
        }
    }

    #[tokio::test]
    async fn complete_with_tool_pops_from_queue_in_order() {
        let stub = ScriptedLlm::new();
        stub.queue_tool(Ok(tool_call(json!({"i": 1}))));
        stub.queue_tool(Ok(tool_call(json!({"i": 2}))));

        let schema = ToolSchema {
            name: "test_tool".to_string(),
            description: "".to_string(),
            input_schema: json!({}),
        };
        let (first, _u) = stub.complete_with_tool("", "", schema.clone(), None).await.unwrap();
        assert_eq!(first.input, json!({"i": 1}));
        let (second, _u) = stub.complete_with_tool("", "", schema, None).await.unwrap();
        assert_eq!(second.input, json!({"i": 2}));
        assert!(stub.is_empty());
    }

    #[tokio::test]
    async fn complete_free_pops_from_queue_in_order() {
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("first".to_string()));
        stub.queue_free(Ok("second".to_string()));

        assert_eq!(stub.complete_free("", &[], None).await.unwrap().0, "first");
        assert_eq!(stub.complete_free("", &[], None).await.unwrap().0, "second");
        assert!(stub.is_empty());
    }

    #[tokio::test]
    async fn clone_shares_queue_state() {
        let stub = ScriptedLlm::new();
        let probe = stub.clone();
        stub.queue_free(Ok("hello".to_string()));
        assert_eq!(probe.remaining(), (0, 1));
        assert_eq!(stub.complete_free("", &[], None).await.unwrap().0, "hello");
        assert!(probe.is_empty());
    }

    #[tokio::test]
    async fn errors_propagate() {
        let stub = ScriptedLlm::new();
        stub.queue_free(Err(LlmError::Retryable {
            reason: "transient".to_string(),
        }));
        stub.queue_tool(Err(LlmError::Fatal {
            reason: FatalReason::SchemaValidation("bad".to_string()),
        }));

        let free_err = stub.complete_free("", &[], None).await.unwrap_err();
        assert!(matches!(free_err, LlmError::Retryable { .. }));

        let schema = ToolSchema {
            name: "t".to_string(),
            description: "".to_string(),
            input_schema: json!({}),
        };
        let tool_err = stub.complete_with_tool("", "", schema, None).await.unwrap_err();
        assert!(matches!(tool_err, LlmError::Fatal { .. }));
    }

    #[tokio::test]
    #[should_panic(expected = "complete_free called with empty queue")]
    async fn complete_free_panics_when_empty() {
        let stub = ScriptedLlm::new();
        let _ = stub.complete_free("", &[], None).await;
    }
}
