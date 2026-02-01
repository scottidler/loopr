//! Anthropic API client implementation
//!
//! This module implements the LlmClient trait for the Anthropic (Claude) API.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use crate::error::{LooprError, Result};
use crate::llm::client::LlmClient;
use crate::llm::types::{
    CompletionRequest, CompletionResponse, Message, Role, StopReason, ToolCall, ToolResult, Usage,
};

/// Anthropic API base URL
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Anthropic API version
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default model to use
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Default max tokens
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Configuration for the Anthropic client
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub model: String,
    pub max_tokens: u32,
    pub timeout: Duration,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: Duration::from_secs(300),
        }
    }
}

impl AnthropicConfig {
    /// Create a new config with a specific model
    pub fn with_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }
}

/// Anthropic API client
pub struct AnthropicClient {
    client: Client,
    api_key: String,
    config: AnthropicConfig,
    usage: Arc<Mutex<Usage>>,
}

impl AnthropicClient {
    /// Create a new Anthropic client
    ///
    /// Reads ANTHROPIC_API_KEY from environment
    pub fn new(config: AnthropicConfig) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| LooprError::Llm("ANTHROPIC_API_KEY not set".to_string()))?;

        Self::with_api_key(api_key, config)
    }

    /// Create a client with an explicit API key
    pub fn with_api_key(api_key: String, config: AnthropicConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| LooprError::Llm(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self {
            client,
            api_key,
            config,
            usage: Arc::new(Mutex::new(Usage::default())),
        })
    }

    /// Build the request body for the Anthropic API
    fn build_request(&self, request: &CompletionRequest) -> Value {
        let model = request
            .model
            .as_ref()
            .unwrap_or(&self.config.model)
            .clone();

        let max_tokens = request.max_tokens.unwrap_or(self.config.max_tokens);

        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                    },
                    "content": m.content
                })
            })
            .collect();

        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": messages
        });

        // Add system prompt if present
        if !request.system.is_empty() {
            body["system"] = json!(request.system);
        }

        // Add tools if present
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request.tools.iter().map(|t| t.to_anthropic_schema()).collect();
            body["tools"] = json!(tools);
        }

        body
    }

    /// Build a request that continues with tool results
    fn build_continuation_request(
        &self,
        request: &CompletionRequest,
        results: &[ToolResult],
    ) -> Value {
        let mut body = self.build_request(request);

        // Add tool results as user messages with tool_result content blocks
        let tool_results: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.tool_use_id,
                    "content": r.content,
                    "is_error": r.is_error
                })
            })
            .collect();

        // Append a user message containing the tool results
        if let Some(messages) = body["messages"].as_array_mut() {
            messages.push(json!({
                "role": "user",
                "content": tool_results
            }));
        }

        body
    }

    /// Parse the API response into a CompletionResponse
    fn parse_response(&self, body: Value) -> Result<CompletionResponse> {
        // Extract stop reason
        let stop_reason = match body["stop_reason"].as_str() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        };

        // Extract usage
        let usage = if let Some(u) = body.get("usage") {
            Usage::new(
                u["input_tokens"].as_u64().unwrap_or(0),
                u["output_tokens"].as_u64().unwrap_or(0),
            )
        } else {
            Usage::default()
        };

        // Track cumulative usage
        {
            let mut total = self.usage.lock().unwrap();
            total.add(&usage);
        }

        // Extract content and tool calls
        let mut content = String::new();
        let mut tool_calls = Vec::new();

        if let Some(blocks) = body["content"].as_array() {
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            if !content.is_empty() {
                                content.push('\n');
                            }
                            content.push_str(text);
                        }
                    }
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap_or("").to_string();
                        let name = block["name"].as_str().unwrap_or("").to_string();
                        let input = block["input"].clone();
                        tool_calls.push(ToolCall::new(id, name, input));
                    }
                    _ => {}
                }
            }
        }

        Ok(CompletionResponse {
            content,
            tool_calls,
            stop_reason,
            usage,
        })
    }

    /// Send a request to the Anthropic API
    async fn send_request(&self, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| LooprError::Llm(format!("Request failed: {}", e)))?;

        let status = response.status();

        // Handle rate limiting
        if status.as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            return Err(LooprError::Llm(format!(
                "Rate limited, retry after {} seconds",
                retry_after
            )));
        }

        // Handle other errors
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(LooprError::Llm(format!(
                "API error {}: {}",
                status, error_body
            )));
        }

        response
            .json()
            .await
            .map_err(|e| LooprError::Llm(format!("Failed to parse response: {}", e)))
    }

    /// Get cumulative token usage
    pub fn total_usage(&self) -> Usage {
        self.usage.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let body = self.build_request(&request);
        let response = self.send_request(body).await?;
        self.parse_response(response)
    }

    async fn continue_with_tool_results(
        &self,
        request: CompletionRequest,
        results: Vec<ToolResult>,
    ) -> Result<CompletionResponse> {
        let body = self.build_continuation_request(&request, &results);
        let response = self.send_request(body).await?;
        self.parse_response(response)
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn is_ready(&self) -> bool {
        !self.api_key.is_empty()
    }
}

// Make AnthropicClient thread-safe for use as trait object
impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("model", &self.config.model)
            .field("max_tokens", &self.config.max_tokens)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = AnthropicConfig::default();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(config.timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_config_with_model() {
        let config = AnthropicConfig::with_model("claude-3-haiku-20240307");
        assert_eq!(config.model, "claude-3-haiku-20240307");
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn test_client_without_api_key() {
        // Temporarily remove the key if it exists
        let original = std::env::var("ANTHROPIC_API_KEY").ok();
        // SAFETY: This test runs single-threaded and restores the var before returning
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let result = AnthropicClient::new(AnthropicConfig::default());
        assert!(result.is_err());

        // Restore
        if let Some(key) = original {
            // SAFETY: Restoring the environment variable to its original state
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", key);
            }
        }
    }

    #[test]
    fn test_client_with_api_key() {
        let result =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default());
        assert!(result.is_ok());
        let client = result.unwrap();
        assert!(client.is_ready());
        assert_eq!(client.model(), DEFAULT_MODEL);
    }

    #[test]
    fn test_build_request_basic() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let request = CompletionRequest::new("You are helpful").with_user_message("Hello");

        let body = client.build_request(&request);

        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["system"], "You are helpful");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
    }

    #[test]
    fn test_build_request_with_tools() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let tool = crate::llm::ToolDefinition::new(
            "read_file",
            "Read a file",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        );

        let request = CompletionRequest::new("test")
            .with_user_message("Read foo.txt")
            .with_tools(vec![tool]);

        let body = client.build_request(&request);

        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["name"], "read_file");
    }

    #[test]
    fn test_build_request_custom_model() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let mut request = CompletionRequest::new("test").with_user_message("Hello");
        request.model = Some("claude-opus-4-5-20250514".to_string());

        let body = client.build_request(&request);

        assert_eq!(body["model"], "claude-opus-4-5-20250514");
    }

    #[test]
    fn test_parse_response_text_only() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let api_response = json!({
            "content": [
                { "type": "text", "text": "Hello there!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });

        let response = client.parse_response(api_response).unwrap();

        assert_eq!(response.content, "Hello there!");
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
    }

    #[test]
    fn test_parse_response_with_tool_use() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let api_response = json!({
            "content": [
                { "type": "text", "text": "Let me read that file" },
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "read_file",
                    "input": { "path": "/tmp/test.txt" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 30
            }
        });

        let response = client.parse_response(api_response).unwrap();

        assert_eq!(response.content, "Let me read that file");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_123");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].input["path"], "/tmp/test.txt");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn test_parse_response_multiple_tool_calls() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let api_response = json!({
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "read_file",
                    "input": { "path": "a.txt" }
                },
                {
                    "type": "tool_use",
                    "id": "toolu_2",
                    "name": "read_file",
                    "input": { "path": "b.txt" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 100, "output_tokens": 50 }
        });

        let response = client.parse_response(api_response).unwrap();

        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "toolu_1");
        assert_eq!(response.tool_calls[1].id, "toolu_2");
    }

    #[test]
    fn test_parse_response_stop_reasons() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let test_cases = vec![
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("unknown", StopReason::EndTurn), // Fallback
        ];

        for (reason_str, expected) in test_cases {
            let api_response = json!({
                "content": [],
                "stop_reason": reason_str,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            });

            let response = client.parse_response(api_response).unwrap();
            assert_eq!(response.stop_reason, expected);
        }
    }

    #[test]
    fn test_total_usage_accumulation() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        // Parse first response
        let _ = client.parse_response(json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 100, "output_tokens": 50 }
        }));

        // Parse second response
        let _ = client.parse_response(json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 200, "output_tokens": 100 }
        }));

        let total = client.total_usage();
        assert_eq!(total.input_tokens, 300);
        assert_eq!(total.output_tokens, 150);
    }

    #[test]
    fn test_build_continuation_request() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let request = CompletionRequest::new("system")
            .with_user_message("Read the file")
            .with_message(Message::assistant("I'll read the file"));

        let results = vec![ToolResult::success("toolu_123", "file contents here")];

        let body = client.build_continuation_request(&request, &results);

        // Should have 3 messages now: user, assistant, user (with tool results)
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);

        // Last message should be user with tool_result content
        let last = &messages[2];
        assert_eq!(last["role"], "user");
        assert!(last["content"].is_array());
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "toolu_123");
        assert_eq!(last["content"][0]["content"], "file contents here");
        assert!(!last["content"][0]["is_error"].as_bool().unwrap());
    }

    #[test]
    fn test_build_continuation_with_error_result() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let request = CompletionRequest::new("system").with_user_message("Read it");

        let results = vec![ToolResult::error("toolu_456", "File not found")];

        let body = client.build_continuation_request(&request, &results);

        let messages = body["messages"].as_array().unwrap();
        let last = &messages[1];
        assert!(last["content"][0]["is_error"].as_bool().unwrap());
        assert_eq!(last["content"][0]["content"], "File not found");
    }

    #[test]
    fn test_debug_impl() {
        let client =
            AnthropicClient::with_api_key("test-key".to_string(), AnthropicConfig::default())
                .unwrap();

        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("AnthropicClient"));
        assert!(debug_str.contains(DEFAULT_MODEL));
        // Should NOT contain the API key
        assert!(!debug_str.contains("test-key"));
    }

    #[test]
    fn test_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AnthropicClient>();
    }

    #[test]
    fn test_empty_api_key_not_ready() {
        let client =
            AnthropicClient::with_api_key(String::new(), AnthropicConfig::default()).unwrap();
        assert!(!client.is_ready());
    }
}
