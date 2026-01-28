//! Core LLM client types and trait definitions

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

/// Stateless LLM client - each call is independent (fresh context)
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Single completion request (blocking until complete)
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError>;

    /// Streaming completion for TUI progress display
    async fn stream(
        &self,
        request: CompletionRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse, LlmError>;
}

/// A completion request - everything needed for one LLM call
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// System prompt (rendered from Handlebars template)
    pub system_prompt: String,

    /// User messages (typically just one for Ralph loops)
    pub messages: Vec<Message>,

    /// Available tools for this loop type
    pub tools: Vec<super::ToolDefinition>,

    /// Max tokens for response (from config)
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// Response from a completion request
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// Text content (if any)
    pub content: Option<String>,

    /// Tool calls requested by the model
    pub tool_calls: Vec<ToolCall>,

    /// Why the model stopped
    pub stop_reason: StopReason,

    /// Token usage for cost tracking
    pub usage: TokenUsage,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl TokenUsage {
    /// Calculate cost in USD
    /// Opus 4.5: $15/$75 per 1M tokens (input/output)
    /// Haiku 3.5: $0.80/$4 per 1M tokens (input/output)
    pub fn cost_usd(&self, model: &str) -> f64 {
        let (input_price, output_price) = match model {
            m if m.contains("opus") => (15.0, 75.0),
            m if m.contains("haiku") => (0.80, 4.0),
            _ => (15.0, 75.0), // Default to opus pricing
        };

        let input_cost = (self.input_tokens as f64 / 1_000_000.0) * input_price;
        let output_cost = (self.output_tokens as f64 / 1_000_000.0) * output_price;

        // Cache reads are 90% cheaper
        let cache_cost = (self.cache_read_tokens as f64 / 1_000_000.0) * input_price * 0.1;

        input_cost + output_cost + cache_cost
    }
}

/// Streaming chunk for real-time TUI updates
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Text being generated
    TextDelta(String),

    /// Tool call starting
    ToolUseStart { id: String, name: String },

    /// Tool call JSON fragment
    ToolUseDelta { id: String, json_delta: String },

    /// Tool call complete
    ToolUseEnd { id: String },

    /// Message complete with final stats
    MessageDone { stop_reason: StopReason, usage: TokenUsage },

    /// Error during streaming
    Error(String),
}

/// Errors that can occur during LLM operations
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Rate limited, retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },

    #[error("API error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("JSON parsing error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Context overflow: used {used} tokens, limit is {limit}")]
    ContextOverflow { used: usize, limit: usize },

    #[error("Missing API key: environment variable {env_var} not set")]
    MissingApiKey { env_var: String },

    #[error("Event source error: {0}")]
    EventSource(String),
}

impl LlmError {
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, LlmError::RateLimited { .. })
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::RateLimited { .. } => true,
            LlmError::ApiError { status, .. } => *status >= 500,
            LlmError::Network(_) => true,
            LlmError::InvalidResponse(_) => false,
            LlmError::JsonError(_) => false,
            LlmError::ContextOverflow { .. } => false,
            LlmError::MissingApiKey { .. } => false,
            LlmError::EventSource(_) => true,
        }
    }
}

/// Rough token estimation (actual tokenization varies)
/// Claude uses ~4 characters per token on average for English text
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Check if prompt fits within budget
pub fn check_context_budget(
    system_prompt: &str,
    user_prompt: &str,
    max_output_tokens: u32,
    model_context_limit: usize,
) -> Result<ContextBudget, LlmError> {
    let system_tokens = estimate_tokens(system_prompt);
    let user_tokens = estimate_tokens(user_prompt);
    let total_input = system_tokens + user_tokens;

    // Reserve space for output + buffer
    let available = model_context_limit
        .saturating_sub(max_output_tokens as usize)
        .saturating_sub(1000); // Safety buffer

    if total_input > available {
        Err(LlmError::ContextOverflow {
            used: total_input,
            limit: available,
        })
    } else {
        Ok(ContextBudget {
            system_tokens,
            user_tokens,
            available_for_output: available - total_input,
        })
    }
}

/// Context budget information
#[derive(Debug, Clone)]
pub struct ContextBudget {
    pub system_tokens: usize,
    pub user_tokens: usize,
    pub available_for_output: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_usage_cost_opus() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: 500_000,
            cache_creation_tokens: 0,
        };

        // Opus 4.5: $15/M input, $75/M output, 90% discount on cache
        let cost = usage.cost_usd("claude-opus-4-5");
        // $15 (input) + $7.50 (output) + $0.75 (cache reads)
        assert!((cost - 23.25).abs() < 0.01);
    }

    #[test]
    fn test_token_usage_cost_haiku() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        // Haiku 3.5: $0.80/M input, $4/M output
        let cost = usage.cost_usd("claude-haiku-3-5");
        // $0.80 (input) + $0.40 (output)
        assert!((cost - 1.20).abs() < 0.01);
    }

    #[test]
    fn test_estimate_tokens() {
        let text = "Hello, world!"; // 13 chars
        let tokens = estimate_tokens(text);
        assert_eq!(tokens, 3); // 13 / 4 = 3
    }

    #[test]
    fn test_context_budget_ok() {
        let result = check_context_budget("You are a helpful assistant.", "Hello!", 4096, 200_000);
        assert!(result.is_ok());
        let budget = result.unwrap();
        assert!(budget.system_tokens > 0);
        assert!(budget.user_tokens > 0);
    }

    #[test]
    fn test_context_budget_overflow() {
        // Create a huge prompt that exceeds the limit
        let huge_prompt = "x".repeat(1_000_000);
        let result = check_context_budget(&huge_prompt, "Hello", 4096, 200_000);
        assert!(matches!(result, Err(LlmError::ContextOverflow { .. })));
    }

    #[test]
    fn test_llm_error_is_retryable() {
        assert!(
            LlmError::RateLimited {
                retry_after: Duration::from_secs(60)
            }
            .is_retryable()
        );

        assert!(
            LlmError::ApiError {
                status: 500,
                message: "Internal error".to_string()
            }
            .is_retryable()
        );

        assert!(
            !LlmError::ApiError {
                status: 400,
                message: "Bad request".to_string()
            }
            .is_retryable()
        );

        assert!(!LlmError::InvalidResponse("bad".to_string()).is_retryable());
    }

    #[test]
    fn test_stop_reason_equality() {
        assert_eq!(StopReason::EndTurn, StopReason::EndTurn);
        assert_ne!(StopReason::EndTurn, StopReason::ToolUse);
    }

    #[test]
    fn test_role_serialization() {
        let user = Role::User;
        let json = serde_json::to_string(&user).unwrap();
        assert_eq!(json, "\"user\"");

        let assistant = Role::Assistant;
        let json = serde_json::to_string(&assistant).unwrap();
        assert_eq!(json, "\"assistant\"");
    }
}
