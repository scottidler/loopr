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
use crate::message::{Message, MessageContent};
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

/// Prompt-content-keyed responses. Each entry is `(needle, response)`;
/// a `complete_*` call selects the first entry whose `needle` is a
/// substring of the incoming prompt haystack (system + user/messages).
/// Insertion-ordered, first-match-wins. Multi-Work tests use this so a
/// response binds to the right Work regardless of dispatch or retry
/// order — `model`-FIFO alone cannot disambiguate Implementer vs.
/// Reviewer (both call `complete_free` with `model: None`).
type ToolKeyed = Vec<(String, Result<ToolCall, LlmError>)>;
type FreeKeyed = Vec<(String, Result<String, LlmError>)>;

#[derive(Clone, Default)]
pub struct ScriptedLlm {
    tool_responses: Arc<Mutex<ToolMap>>,
    free_responses: Arc<Mutex<FreeMap>>,
    tool_keyed: Arc<Mutex<ToolKeyed>>,
    free_keyed: Arc<Mutex<FreeKeyed>>,
    /// `Usage` returned with every response. Defaults to all-zero /
    /// model-`None`; metering tests call `set_usage` to script real token
    /// counts + a model id (finding 7).
    usage: Arc<Mutex<Usage>>,
}

/// Build the substring-match haystack from a system prompt plus all
/// `Text` content blocks across a message slice. Non-text blocks
/// (`ToolUse` / `ToolResult`) are skipped — they carry no prompt text a
/// test would key on.
fn messages_haystack(system: &str, messages: &[Message]) -> String {
    let mut haystack = String::from(system);
    for message in messages {
        for content in &message.content {
            if let MessageContent::Text { text } = content {
                haystack.push('\n');
                haystack.push_str(text);
            }
        }
    }
    haystack
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

    /// Queue a free-form response keyed by prompt content: it is returned
    /// by the first `complete_free` call whose haystack (system + message
    /// text) contains `needle`. Selected before the model-FIFO queues.
    pub fn queue_free_keyed(&self, needle: &str, result: Result<String, LlmError>) {
        self.free_keyed
            .lock()
            .expect("free_keyed lock")
            .push((needle.to_string(), result));
    }

    /// Queue a tool response keyed by prompt content: returned by the
    /// first `complete_with_tool` call whose haystack (system + user)
    /// contains `needle`. Selected before the model-FIFO queues.
    pub fn queue_tool_keyed(&self, needle: &str, result: Result<ToolCall, LlmError>) {
        self.tool_keyed
            .lock()
            .expect("tool_keyed lock")
            .push((needle.to_string(), result));
    }

    /// Script the `Usage` returned with every subsequent response.
    /// Lets a metering test assert that token counts + the response model
    /// flow through `MeteredLlmClient` / costs.jsonl. Default is
    /// `Usage::default()` (all zero, `model = None`).
    pub fn set_usage(&self, usage: Usage) {
        *self.usage.lock().expect("usage lock") = usage;
    }

    fn usage(&self) -> Usage {
        self.usage.lock().expect("usage lock").clone()
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
            .sum::<usize>()
            + self.tool_keyed.lock().expect("tool_keyed lock").len();
        let f: usize = self
            .free_responses
            .lock()
            .expect("free_responses lock")
            .values()
            .map(|q| q.len())
            .sum::<usize>()
            + self.free_keyed.lock().expect("free_keyed lock").len();
        (t, f)
    }
}

/// Pop the first keyed entry whose needle is a substring of `haystack`,
/// returning its response. `None` when no needle matches — the caller
/// then falls back to the model-FIFO queue.
fn take_keyed<T>(keyed: &Mutex<Vec<(String, Result<T, LlmError>)>>, haystack: &str) -> Option<Result<T, LlmError>> {
    let mut entries = keyed.lock().expect("keyed lock");
    let pos = entries
        .iter()
        .position(|(needle, _)| haystack.contains(needle.as_str()))?;
    Some(entries.remove(pos).1)
}

impl LlmClient for ScriptedLlm {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        _tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        // Keyed responses take precedence: match the queued needle against
        // the system + user prompt before falling back to model-FIFO.
        let haystack = format!("{system}\n{user}");
        if let Some(result) = take_keyed(&self.tool_keyed, &haystack) {
            let usage = self.usage();
            return result.map(|tc| (tc, usage));
        }
        let key: ModelKey = model.map(|m| m.to_string());
        let popped = {
            let mut map = self.tool_responses.lock().expect("tool_responses lock");
            map.get_mut(&key).and_then(|q| q.pop_front())
        };
        match popped {
            Some(Ok(tc)) => Ok((tc, self.usage())),
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
        system: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        // Keyed responses take precedence: match the queued needle against
        // the system prompt + all message text before model-FIFO fallback.
        let haystack = messages_haystack(system, messages);
        if let Some(result) = take_keyed(&self.free_keyed, &haystack) {
            let usage = self.usage();
            return result.map(|s| (s, usage));
        }
        // Model-FIFO: an exact `Some(model)` queue (via `queue_free_for`)
        // takes precedence when populated; otherwise fall back to the
        // `None`-keyed default queue (via `queue_free`). Phase 13 of
        // `docs/design/2026-07-11-verified-swarm.md` made the Implementer
        // and Reviewer pass their own resolved `Some(model)` (previously
        // always `None`), so `None` is now the general-purpose default
        // route rather than a route only the Director's callers skip --
        // existing tests that queue via the untargeted `queue_free` keep
        // working unmodified regardless of which role's resolved model is
        // passed at the call site.
        let key: ModelKey = model.map(|m| m.to_string());
        let popped = {
            let mut map = self.free_responses.lock().expect("free_responses lock");
            map.get_mut(&key)
                .and_then(|q| q.pop_front())
                .or_else(|| map.get_mut(&None).and_then(|q| q.pop_front()))
        };
        match popped {
            Some(Ok(s)) => Ok((s, self.usage())),
            Some(Err(e)) => Err(e),
            None => {
                let (t, f) = self.remaining();
                panic!(
                    "ScriptedLlm: complete_free called with empty queue for model={model:?} (tool remaining: {t}, free remaining: {f})"
                );
            }
        }
    }

    fn model(&self) -> &str {
        "scripted-stub-model"
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
            reason: crate::error::RetryableReason::Network {
                detail: "transient".to_string(),
            },
        }));
        stub.queue_tool(Err(LlmError::Fatal {
            reason: FatalReason::SchemaValidation {
                message: "bad".to_string(),
                usage: Usage::default(),
            },
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
    async fn set_usage_is_returned_with_responses() {
        let stub = ScriptedLlm::new();
        stub.set_usage(Usage {
            input_tokens: 123,
            output_tokens: 45,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            model: Some("claude-sonnet-4-6-20260115".to_string()),
        });
        stub.queue_free(Ok("ok".to_string()));
        let (_out, usage) = stub.complete_free("s", &[], None).await.unwrap();
        assert_eq!(usage.input_tokens, 123);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-4-6-20260115"));
    }

    #[tokio::test]
    #[should_panic(expected = "complete_free called with empty queue")]
    async fn complete_free_panics_when_empty() {
        let stub = ScriptedLlm::new();
        let _ = stub.complete_free("", &[], None).await;
    }

    #[tokio::test]
    async fn free_keyed_selected_before_model_fifo() {
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("fifo".to_string()));
        stub.queue_free_keyed("WORK-A", Ok("keyed-a".to_string()));

        // Prompt text contains the needle: the keyed entry wins over FIFO.
        let messages = [Message::user("implement WORK-A now")];
        assert_eq!(stub.complete_free("sys", &messages, None).await.unwrap().0, "keyed-a");
        // Next call has no matching needle: falls back to the FIFO queue.
        assert_eq!(
            stub.complete_free("sys", &[Message::user("WORK-B")], None)
                .await
                .unwrap()
                .0,
            "fifo"
        );
        assert!(stub.is_empty());
    }

    #[tokio::test]
    async fn free_keyed_matches_against_system_prompt_too() {
        let stub = ScriptedLlm::new();
        stub.queue_free_keyed("you are the reviewer", Ok("review-verdict".to_string()));
        // Needle lives in the system prompt, not the messages.
        let got = stub
            .complete_free("you are the reviewer", &[Message::user("here is the diff")], None)
            .await
            .unwrap()
            .0;
        assert_eq!(got, "review-verdict");
    }

    #[tokio::test]
    async fn free_keyed_no_match_leaves_entry_and_uses_fifo() {
        let stub = ScriptedLlm::new();
        stub.queue_free_keyed("NEVER", Ok("keyed".to_string()));
        stub.queue_free(Ok("fifo".to_string()));
        assert_eq!(
            stub.complete_free("", &[Message::user("nothing here")], None)
                .await
                .unwrap()
                .0,
            "fifo"
        );
        // The unmatched keyed entry is untouched.
        assert_eq!(stub.remaining(), (0, 1));
    }

    #[tokio::test]
    async fn free_keyed_first_match_wins() {
        let stub = ScriptedLlm::new();
        stub.queue_free_keyed("WORK", Ok("first".to_string()));
        stub.queue_free_keyed("WORK", Ok("second".to_string()));
        let messages = [Message::user("WORK item")];
        assert_eq!(stub.complete_free("", &messages, None).await.unwrap().0, "first");
        assert_eq!(stub.complete_free("", &messages, None).await.unwrap().0, "second");
        assert!(stub.is_empty());
    }

    // Phase 13 (`docs/design/2026-07-11-verified-swarm.md`, per-role model
    // routing) made the Implementer and Reviewer pass their own resolved
    // `Some(model)` to `complete_free` instead of always `None`. Without a
    // fallback, an untargeted `queue_free` (routed to the `None` key)
    // would never be found by a `Some("claude-...")` lookup and every
    // existing implementer/reviewer test would panic on an empty queue.
    #[tokio::test]
    async fn complete_free_falls_back_to_untargeted_queue_when_model_specific_queue_is_empty() {
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("default-route".to_string()));
        assert_eq!(
            stub.complete_free("", &[], Some("claude-sonnet-4-6")).await.unwrap().0,
            "default-route"
        );
        assert!(stub.is_empty());
    }

    #[tokio::test]
    async fn complete_free_prefers_model_specific_queue_over_untargeted_fallback() {
        let stub = ScriptedLlm::new();
        stub.queue_free(Ok("default-route".to_string()));
        stub.queue_free_for("claude-haiku-4-5", Ok("lightweight-route".to_string()));
        assert_eq!(
            stub.complete_free("", &[], Some("claude-haiku-4-5")).await.unwrap().0,
            "lightweight-route"
        );
        // The untargeted queue is untouched by the model-specific hit.
        assert_eq!(stub.remaining(), (0, 1));
        assert_eq!(
            stub.complete_free("", &[], Some("claude-haiku-4-5")).await.unwrap().0,
            "default-route"
        );
        assert!(stub.is_empty());
    }

    #[tokio::test]
    async fn tool_keyed_selected_before_model_fifo() {
        let stub = ScriptedLlm::new();
        stub.queue_tool(Ok(tool_call(json!({"route": "fifo"}))));
        stub.queue_tool_keyed("decompose plan-a", Ok(tool_call(json!({"route": "keyed"}))));
        let schema = ToolSchema {
            name: "t".to_string(),
            description: "".to_string(),
            input_schema: json!({}),
        };
        let (keyed, _u) = stub
            .complete_with_tool("sys", "decompose plan-a into works", schema.clone(), None)
            .await
            .unwrap();
        assert_eq!(keyed.input, json!({"route": "keyed"}));
        let (fifo, _u) = stub.complete_with_tool("sys", "unrelated", schema, None).await.unwrap();
        assert_eq!(fifo.input, json!({"route": "fifo"}));
        assert!(stub.is_empty());
    }
}
