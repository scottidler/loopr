//! Scripted `LlmClient` stub for tests.
//!
//! Centralized here so tests across `agents`, `decomposer`, and `loopr`
//! share one implementation. Gated behind the `stub` cargo feature so the
//! production `loopr` binary never links it.
//!
//! Queues live behind `Arc<Mutex<_>>` and the struct is `Clone`: a test
//! can clone the stub BEFORE handing it to a daemon or agent and retain a
//! probe for post-run assertions (e.g. assert the queue was fully drained).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::client::LlmClient;
use crate::error::LlmError;
use crate::message::Message;
use crate::tool::{ToolCall, ToolSchema};
use crate::usage::Usage;

#[derive(Clone, Default)]
pub struct ScriptedLlm {
    tool_responses: Arc<Mutex<VecDeque<Result<ToolCall, LlmError>>>>,
    free_responses: Arc<Mutex<VecDeque<Result<String, LlmError>>>>,
}

impl ScriptedLlm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue_tool(&self, result: Result<ToolCall, LlmError>) {
        self.tool_responses
            .lock()
            .expect("tool_responses lock")
            .push_back(result);
    }

    pub fn queue_free(&self, result: Result<String, LlmError>) {
        self.free_responses
            .lock()
            .expect("free_responses lock")
            .push_back(result);
    }

    pub fn is_empty(&self) -> bool {
        let (t, f) = self.remaining();
        t == 0 && f == 0
    }

    pub fn remaining(&self) -> (usize, usize) {
        let t = self.tool_responses.lock().expect("tool_responses lock").len();
        let f = self.free_responses.lock().expect("free_responses lock").len();
        (t, f)
    }
}

impl LlmClient for ScriptedLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: ToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        let popped = self.tool_responses.lock().expect("tool_responses lock").pop_front();
        match popped {
            Some(Ok(tc)) => Ok((tc, Usage::default())),
            Some(Err(e)) => Err(e),
            None => {
                let (t, f) = self.remaining();
                panic!(
                    "ScriptedLlm: complete_with_tool called with empty queue (tool remaining: {t}, free remaining: {f})"
                );
            }
        }
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        let popped = self.free_responses.lock().expect("free_responses lock").pop_front();
        match popped {
            Some(Ok(s)) => Ok((s, Usage::default())),
            Some(Err(e)) => Err(e),
            None => {
                let (t, f) = self.remaining();
                panic!("ScriptedLlm: complete_free called with empty queue (tool remaining: {t}, free remaining: {f})");
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
