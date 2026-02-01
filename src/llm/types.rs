//! LLM types for Anthropic API communication
//!
//! This module defines all the message types for LLM requests and responses.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Role in a conversation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    /// Create a user message
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Create an assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Tool definition for the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    /// Create a new tool definition
    pub fn new(name: impl Into<String>, description: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    /// Convert to Anthropic API schema format
    pub fn to_anthropic_schema(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema
        })
    }
}

/// A tool call from the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl ToolCall {
    /// Create a new tool call
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
        }
    }
}

/// Result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    /// Create a successful tool result
    pub fn success(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// Create an error tool result
    pub fn error(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: true,
        }
    }
}

/// Request to the LLM for completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Default for CompletionRequest {
    fn default() -> Self {
        Self {
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: None,
            model: None,
        }
    }
}

impl CompletionRequest {
    /// Create a new completion request with a system prompt
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            ..Default::default()
        }
    }

    /// Add a message to the request
    pub fn with_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Add a user message
    pub fn with_user_message(self, content: impl Into<String>) -> Self {
        self.with_message(Message::user(content))
    }

    /// Add tools to the request
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    /// Set max tokens
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

/// Response from the LLM
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Reason why the LLM stopped generating
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

impl StopReason {
    /// Check if the stop reason indicates more work is needed
    pub fn needs_continuation(&self) -> bool {
        matches!(self, StopReason::ToolUse)
    }
}

/// Token usage statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    /// Create new usage stats
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    /// Calculate total tokens
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Accumulate usage from another instance
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }

    /// Calculate cost in USD based on model
    pub fn cost_usd(&self, model: &str) -> f64 {
        let (input_rate, output_rate) = match model {
            m if m.contains("opus") => (0.015, 0.075),
            m if m.contains("sonnet") => (0.003, 0.015),
            m if m.contains("haiku") => (0.00025, 0.00125),
            _ => (0.003, 0.015),
        };

        (self.input_tokens as f64 / 1000.0 * input_rate)
            + (self.output_tokens as f64 / 1000.0 * output_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_serialization() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
    }

    #[test]
    fn test_role_deserialization() {
        let user: Role = serde_json::from_str("\"user\"").unwrap();
        let assistant: Role = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(user, Role::User);
        assert_eq!(assistant, Role::Assistant);
    }

    #[test]
    fn test_message_user() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "Hello");
    }

    #[test]
    fn test_message_assistant() {
        let msg = Message::assistant("Hi there");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "Hi there");
    }

    #[test]
    fn test_tool_definition_to_anthropic_schema() {
        let tool = ToolDefinition::new(
            "read_file",
            "Read file contents",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        );

        let schema = tool.to_anthropic_schema();
        assert_eq!(schema["name"], "read_file");
        assert_eq!(schema["description"], "Read file contents");
        assert!(schema["input_schema"].is_object());
    }

    #[test]
    fn test_tool_call_new() {
        let call = ToolCall::new("call_123", "bash", serde_json::json!({"command": "ls"}));
        assert_eq!(call.id, "call_123");
        assert_eq!(call.name, "bash");
        assert_eq!(call.input["command"], "ls");
    }

    #[test]
    fn test_tool_result_success() {
        let result = ToolResult::success("call_123", "file contents here");
        assert_eq!(result.tool_use_id, "call_123");
        assert_eq!(result.content, "file contents here");
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_result_error() {
        let result = ToolResult::error("call_123", "file not found");
        assert_eq!(result.tool_use_id, "call_123");
        assert_eq!(result.content, "file not found");
        assert!(result.is_error);
    }

    #[test]
    fn test_completion_request_default() {
        let req = CompletionRequest::default();
        assert!(req.system.is_empty());
        assert!(req.messages.is_empty());
        assert!(req.tools.is_empty());
        assert!(req.max_tokens.is_none());
    }

    #[test]
    fn test_completion_request_builder() {
        let req = CompletionRequest::new("You are a helpful assistant")
            .with_user_message("Hello")
            .with_max_tokens(1000);

        assert_eq!(req.system, "You are a helpful assistant");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content, "Hello");
        assert_eq!(req.max_tokens, Some(1000));
    }

    #[test]
    fn test_stop_reason_needs_continuation() {
        assert!(!StopReason::EndTurn.needs_continuation());
        assert!(StopReason::ToolUse.needs_continuation());
        assert!(!StopReason::MaxTokens.needs_continuation());
        assert!(!StopReason::StopSequence.needs_continuation());
    }

    #[test]
    fn test_stop_reason_default() {
        let reason = StopReason::default();
        assert_eq!(reason, StopReason::EndTurn);
    }

    #[test]
    fn test_usage_total() {
        let usage = Usage::new(100, 50);
        assert_eq!(usage.total(), 150);
    }

    #[test]
    fn test_usage_add() {
        let mut usage1 = Usage::new(100, 50);
        let usage2 = Usage::new(200, 100);
        usage1.add(&usage2);
        assert_eq!(usage1.input_tokens, 300);
        assert_eq!(usage1.output_tokens, 150);
    }

    #[test]
    fn test_usage_cost_sonnet() {
        let usage = Usage::new(1000, 1000);
        let cost = usage.cost_usd("claude-sonnet-4-20250514");
        // input: 1000/1000 * 0.003 = 0.003
        // output: 1000/1000 * 0.015 = 0.015
        // total: 0.018
        assert!((cost - 0.018).abs() < 0.0001);
    }

    #[test]
    fn test_usage_cost_opus() {
        let usage = Usage::new(1000, 1000);
        let cost = usage.cost_usd("claude-opus-4-5-20250514");
        // input: 1000/1000 * 0.015 = 0.015
        // output: 1000/1000 * 0.075 = 0.075
        // total: 0.09
        assert!((cost - 0.09).abs() < 0.0001);
    }

    #[test]
    fn test_usage_cost_haiku() {
        let usage = Usage::new(1000, 1000);
        let cost = usage.cost_usd("claude-3-haiku-20240307");
        // input: 1000/1000 * 0.00025 = 0.00025
        // output: 1000/1000 * 0.00125 = 0.00125
        // total: 0.0015
        assert!((cost - 0.0015).abs() < 0.0001);
    }

    #[test]
    fn test_completion_response_default() {
        let resp = CompletionResponse::default();
        assert!(resp.content.is_empty());
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.total(), 0);
    }
}
