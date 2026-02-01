//! LlmClient trait for LLM API abstraction
//!
//! This module defines the async trait for interacting with LLM providers.

use async_trait::async_trait;

use crate::error::Result;
use crate::llm::types::{CompletionRequest, CompletionResponse, ToolResult};

/// Trait for LLM API clients
///
/// This trait abstracts over different LLM providers (Anthropic, OpenAI, etc.)
/// and enables mocking for tests.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a completion request and get a response
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    /// Continue a conversation with tool results
    ///
    /// This is used when the LLM returns tool_use blocks - we execute the tools,
    /// then call this method to continue the conversation with the results.
    async fn continue_with_tool_results(
        &self,
        request: CompletionRequest,
        results: Vec<ToolResult>,
    ) -> Result<CompletionResponse>;

    /// Get the model name being used
    fn model(&self) -> &str;

    /// Check if the client is configured and ready
    fn is_ready(&self) -> bool;
}

/// Mock LLM client for testing
#[derive(Debug, Default)]
pub struct MockLlmClient {
    pub responses: std::sync::Mutex<Vec<CompletionResponse>>,
    pub model_name: String,
}

impl MockLlmClient {
    /// Create a new mock client
    pub fn new() -> Self {
        Self {
            responses: std::sync::Mutex::new(Vec::new()),
            model_name: "mock-model".to_string(),
        }
    }

    /// Queue a response to be returned by the next complete() call
    pub fn queue_response(&self, response: CompletionResponse) {
        self.responses.lock().unwrap().push(response);
    }

    /// Queue multiple responses
    pub fn queue_responses(&self, responses: Vec<CompletionResponse>) {
        let mut guard = self.responses.lock().unwrap();
        for resp in responses {
            guard.push(resp);
        }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            Ok(CompletionResponse::default())
        } else {
            Ok(guard.remove(0))
        }
    }

    async fn continue_with_tool_results(
        &self,
        _request: CompletionRequest,
        _results: Vec<ToolResult>,
    ) -> Result<CompletionResponse> {
        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            Ok(CompletionResponse::default())
        } else {
            Ok(guard.remove(0))
        }
    }

    fn model(&self) -> &str {
        &self.model_name
    }

    fn is_ready(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{StopReason, ToolCall, Usage};

    #[tokio::test]
    async fn test_mock_client_default_response() {
        let client = MockLlmClient::new();
        let request = CompletionRequest::new("test system").with_user_message("hello");

        let response = client.complete(request).await.unwrap();

        assert!(response.content.is_empty());
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn test_mock_client_queued_response() {
        let client = MockLlmClient::new();

        let expected = CompletionResponse {
            content: "Hello there!".to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::new(10, 5),
        };
        client.queue_response(expected.clone());

        let request = CompletionRequest::new("test").with_user_message("hi");
        let response = client.complete(request).await.unwrap();

        assert_eq!(response.content, "Hello there!");
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn test_mock_client_multiple_responses() {
        let client = MockLlmClient::new();

        client.queue_responses(vec![
            CompletionResponse {
                content: "First".to_string(),
                ..Default::default()
            },
            CompletionResponse {
                content: "Second".to_string(),
                ..Default::default()
            },
        ]);

        let req = CompletionRequest::default();

        let r1 = client.complete(req.clone()).await.unwrap();
        let r2 = client.complete(req.clone()).await.unwrap();
        let r3 = client.complete(req).await.unwrap();

        assert_eq!(r1.content, "First");
        assert_eq!(r2.content, "Second");
        assert!(r3.content.is_empty()); // Default when queue empty
    }

    #[tokio::test]
    async fn test_mock_client_with_tool_calls() {
        let client = MockLlmClient::new();

        let expected = CompletionResponse {
            content: String::new(),
            tool_calls: vec![ToolCall::new(
                "call_1",
                "read_file",
                serde_json::json!({"path": "/tmp/test.txt"}),
            )],
            stop_reason: StopReason::ToolUse,
            usage: Usage::new(50, 20),
        };
        client.queue_response(expected);

        let response = client
            .complete(CompletionRequest::default())
            .await
            .unwrap();

        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn test_mock_client_continue_with_tool_results() {
        let client = MockLlmClient::new();

        client.queue_response(CompletionResponse {
            content: "File contained: hello".to_string(),
            ..Default::default()
        });

        let results = vec![ToolResult::success("call_1", "hello")];
        let response = client
            .continue_with_tool_results(CompletionRequest::default(), results)
            .await
            .unwrap();

        assert_eq!(response.content, "File contained: hello");
    }

    #[test]
    fn test_mock_client_model() {
        let client = MockLlmClient::new();
        assert_eq!(client.model(), "mock-model");
    }

    #[test]
    fn test_mock_client_is_ready() {
        let client = MockLlmClient::new();
        assert!(client.is_ready());
    }

    #[test]
    fn test_mock_client_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockLlmClient>();
    }

    #[tokio::test]
    async fn test_mock_client_trait_object() {
        let mock = MockLlmClient::new();
        mock.queue_response(CompletionResponse {
            content: "via trait object".to_string(),
            ..Default::default()
        });

        let client: Box<dyn LlmClient> = Box::new(mock);
        let response = client
            .complete(CompletionRequest::default())
            .await
            .unwrap();
        assert_eq!(response.content, "via trait object");
    }
}
