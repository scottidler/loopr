# LLM Client Specification

**Author:** Scott A. Idler
**Date:** 2026-01-15 (updated 2026-01-25)
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

The LLM client provides stateless completion requests for Ralph loops. Each iteration gets fresh context - no conversation state carried between calls. Adapted from Neuraphage's production-tested implementation.

---

## Core Trait

```rust
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Stateless LLM client - each call is independent (fresh context)
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Single completion request (blocking until complete)
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    /// Streaming completion for TUI progress display
    async fn stream(
        &self,
        request: CompletionRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse>;
}
```

---

## Request/Response Types

```rust
/// A completion request - everything needed for one LLM call
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// System prompt (rendered from Handlebars template)
    pub system_prompt: String,

    /// User messages (typically just one for Ralph loops)
    pub messages: Vec<Message>,

    /// Available tools for this loop type
    pub tools: Vec<ToolDefinition>,

    /// Max tokens for response (from config)
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq)]
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
```

---

## Streaming Types

```rust
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
    MessageDone {
        stop_reason: StopReason,
        usage: TokenUsage,
    },

    /// Error during streaming
    Error(String),
}
```

---

## AnthropicClient Implementation

```rust
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use futures::StreamExt;

pub struct AnthropicClient {
    model: String,
    api_key: String,
    base_url: String,
    http: Client,
    max_tokens: u32,
    timeout: Duration,
}

impl AnthropicClient {
    pub fn from_config(config: &LlmConfig) -> Result<Self> {
        let api_key = std::env::var(&config.api_key_env)
            .context(format!("Environment variable {} not set", config.api_key_env))?;

        let http = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()?;

        Ok(Self {
            model: config.model.clone(),
            api_key,
            base_url: config.base_url.clone(),
            http,
            max_tokens: config.max_tokens,
            timeout: Duration::from_millis(config.timeout_ms),
        })
    }

    fn build_request_body(&self, request: &CompletionRequest) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens.min(self.max_tokens),
            "system": request.system_prompt,
            "messages": request.messages,
            "tools": request.tools.iter().map(|t| t.to_anthropic_schema()).collect::<Vec<_>>(),
        })
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = self.build_request_body(&request);

        let response = self.http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if response.status() == 429 {
            // Rate limited - return error for caller to handle
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);

            return Err(LlmError::RateLimited {
                retry_after: Duration::from_secs(retry_after),
            }.into());
        }

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::ApiError {
                status: status.as_u16(),
                message: text,
            }.into());
        }

        let api_response: AnthropicResponse = response.json().await?;
        Ok(api_response.into())
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);

        let request = self.http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        let mut es = EventSource::new(request)?;

        let mut full_content = String::new();
        let mut tool_calls = Vec::new();
        let mut current_tool: Option<(String, String, String)> = None; // (id, name, json)
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = TokenUsage::default();

        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Message(msg)) => {
                    let data: serde_json::Value = serde_json::from_str(&msg.data)?;

                    match data["type"].as_str() {
                        Some("content_block_start") => {
                            if let Some(block) = data.get("content_block") {
                                if block["type"] == "tool_use" {
                                    let id = block["id"].as_str().unwrap_or("").to_string();
                                    let name = block["name"].as_str().unwrap_or("").to_string();
                                    current_tool = Some((id.clone(), name.clone(), String::new()));
                                    let _ = chunk_tx.send(StreamChunk::ToolUseStart { id, name }).await;
                                }
                            }
                        }
                        Some("content_block_delta") => {
                            if let Some(delta) = data.get("delta") {
                                if let Some(text) = delta["text"].as_str() {
                                    full_content.push_str(text);
                                    let _ = chunk_tx.send(StreamChunk::TextDelta(text.to_string())).await;
                                }
                                if let Some(json) = delta["partial_json"].as_str() {
                                    if let Some((id, _, ref mut acc)) = current_tool {
                                        acc.push_str(json);
                                        let _ = chunk_tx.send(StreamChunk::ToolUseDelta {
                                            id: id.clone(),
                                            json_delta: json.to_string(),
                                        }).await;
                                    }
                                }
                            }
                        }
                        Some("content_block_stop") => {
                            if let Some((id, name, json)) = current_tool.take() {
                                let input: serde_json::Value = serde_json::from_str(&json)
                                    .unwrap_or(serde_json::json!({}));
                                tool_calls.push(ToolCall { id: id.clone(), name, input });
                                let _ = chunk_tx.send(StreamChunk::ToolUseEnd { id }).await;
                            }
                        }
                        Some("message_delta") => {
                            if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                                stop_reason = match sr {
                                    "end_turn" => StopReason::EndTurn,
                                    "tool_use" => StopReason::ToolUse,
                                    "max_tokens" => StopReason::MaxTokens,
                                    "stop_sequence" => StopReason::StopSequence,
                                    _ => StopReason::EndTurn,
                                };
                            }
                            if let Some(u) = data.get("usage") {
                                usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                            }
                        }
                        Some("message_start") => {
                            if let Some(u) = data["message"].get("usage") {
                                usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                                usage.cache_read_tokens = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                                usage.cache_creation_tokens = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Open) => {}
                Err(e) => {
                    let _ = chunk_tx.send(StreamChunk::Error(e.to_string())).await;
                    break;
                }
            }
        }

        let _ = chunk_tx.send(StreamChunk::MessageDone {
            stop_reason: stop_reason.clone(),
            usage: usage.clone(),
        }).await;

        Ok(CompletionResponse {
            content: if full_content.is_empty() { None } else { Some(full_content) },
            tool_calls,
            stop_reason,
            usage,
        })
    }
}
```

---

## Error Types

```rust
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
        }
    }
}
```

---

## Context Budget Management

Each Claude model has a maximum context window. When prompt content approaches this limit, Loopr must truncate gracefully to avoid API errors.

### Context Limits by Model

| Model | Context Window | Safe Budget (90%) |
|-------|----------------|-------------------|
| claude-opus-4-5-20250514 | 200K tokens | 180K tokens |
| claude-haiku-3-5-20250514 | 200K tokens | 180K tokens |

### Token Estimation

Approximate token count before sending:

```rust
/// Rough token estimation (actual tokenization varies)
/// Claude uses ~4 characters per token on average for English text
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Check if prompt fits within budget
fn check_context_budget(
    system_prompt: &str,
    user_prompt: &str,
    max_output_tokens: u32,
    model_context_limit: usize,
) -> Result<ContextBudget> {
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
```

### Truncation Strategy

When context is exceeded, truncate in priority order:

1. **Progress history** - Oldest iterations first (keep most recent 2-3)
2. **Tool output** - Long file contents, command outputs
3. **Artifact content** - Summarize instead of including full text
4. **Base prompt** - Last resort, never truncate

```rust
fn fit_to_budget(
    components: &mut PromptComponents,
    budget: usize,
) -> Result<()> {
    let mut current_size = components.total_tokens();

    // 1. Truncate progress history
    while current_size > budget && components.progress.len() > 2 {
        let removed = components.progress.remove(0); // Remove oldest
        current_size -= estimate_tokens(&removed);
        tracing::debug!("Truncated oldest progress entry to fit budget");
    }

    // 2. Summarize large artifacts
    if current_size > budget {
        for artifact in &mut components.artifacts {
            if estimate_tokens(artifact) > 5000 {
                *artifact = summarize_artifact(artifact)?;
                current_size = components.total_tokens();
            }
        }
    }

    // 3. Truncate tool outputs
    if current_size > budget {
        for output in &mut components.tool_outputs {
            if output.len() > 10000 {
                *output = format!(
                    "{}...\n[truncated, {} chars total]",
                    &output[..8000],
                    output.len()
                );
                current_size = components.total_tokens();
            }
        }
    }

    if current_size > budget {
        return Err(eyre!(
            "Cannot fit prompt within budget after truncation. \
             Base prompt may be too large ({} tokens for {} budget)",
            current_size,
            budget
        ));
    }

    Ok(())
}
```

### Per-Loop-Type Budgets

Different loop types have different content sizes:

| Loop Type | Typical Input Size | Notes |
|-----------|-------------------|-------|
| Plan | 5-15K tokens | User request + conversation history |
| Spec | 10-20K tokens | Plan content + requirements |
| Phase | 15-30K tokens | Spec content + progress + file context |
| Ralph | 20-50K tokens | Phase + code files + test output |

### Handling Overflow

When a prompt exceeds budget even after truncation:

```rust
async fn handle_context_overflow(
    loop_id: &str,
    overflow: &ContextOverflow,
) -> Result<()> {
    tracing::error!(
        loop_id,
        used = overflow.used,
        limit = overflow.limit,
        "Context budget exceeded"
    );

    // Options:
    // 1. Split the task into smaller chunks
    // 2. Use a model with larger context
    // 3. Fail with actionable error message

    Err(eyre!(
        "Loop {} prompt too large ({} tokens, limit {}). \
         Consider splitting the phase into smaller tasks.",
        loop_id,
        overflow.used,
        overflow.limit
    ))
}
```

---

## Integration with Loop Engine

The loop engine uses `LlmClient` for each iteration:

```rust
async fn run_iteration(
    llm: &dyn LlmClient,
    loop_config: &LoopConfig,
    context: &LoopContext,
) -> Result<IterationResult> {
    // 1. Render prompt template with Handlebars
    let system_prompt = render_system_prompt(loop_config)?;
    let user_prompt = render_user_prompt(loop_config, context)?;

    // 2. Build request (fresh context - no history!)
    let request = CompletionRequest {
        system_prompt,
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(user_prompt),
        }],
        tools: loop_config.tools.clone(),
        max_tokens: loop_config.max_tokens,
    };

    // 3. Call LLM (stateless)
    let response = llm.complete(request).await?;

    // 4. Execute tool calls
    let tool_results = execute_tools(&response.tool_calls, context).await?;

    // 5. If tools were called, continue the turn
    if !tool_results.is_empty() {
        // Build follow-up with tool results
        let follow_up = build_tool_result_request(
            &system_prompt,
            &user_prompt,
            &response,
            &tool_results,
            &loop_config.tools,
        );
        let final_response = llm.complete(follow_up).await?;
        // Continue until end_turn or max_tokens
    }

    Ok(IterationResult {
        usage: response.usage,
        // ...
    })
}
```

---

## Configuration

From [loop-config.md](loop-config.md):

```yaml
llm:
  default: anthropic/claude-opus-4-5-20250514  # Prefer opus for complex tasks
  simple: anthropic/claude-haiku-3-5-20250514  # Use haiku for simple/fast tasks
  timeout-ms: 300000
  providers:
    anthropic:
      api-key-env: ANTHROPIC_API_KEY
      base-url: https://api.anthropic.com
      models:
        claude-opus-4-5-20250514:
          max-tokens: 16384
        claude-haiku-3-5-20250514:
          max-tokens: 8192
```

---

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Mock client for unit tests
    pub struct MockLlmClient {
        responses: Vec<CompletionResponse>,
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl MockLlmClient {
        pub fn new(responses: Vec<CompletionResponse>) -> Self {
            Self {
                responses,
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlmClient {
        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
            let idx = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.responses.get(idx).cloned().ok_or_else(|| eyre!("No more responses"))
        }

        async fn stream(
            &self,
            request: CompletionRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
        ) -> Result<CompletionResponse> {
            self.complete(request).await
        }
    }

    #[tokio::test]
    async fn test_token_usage_cost() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: 500_000,
            cache_creation_tokens: 0,
        };

        // Opus 4.5: $15/M input, $75/M output, 90% discount on cache
        let cost = usage.cost_usd("claude-opus-4-5");
        assert!((cost - 23.25).abs() < 0.01); // $15 + $7.50 + $0.75
    }
}
```

---

## Dependencies

Add to `Cargo.toml`:

```toml
[dependencies]
reqwest = { version = "0.12", features = ["json", "stream"] }
reqwest-eventsource = "0.7"
futures = "0.3"
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and storage
- [loop-config.md](loop-config.md) - LLM configuration
- [tools.md](tools.md) - Tool definitions
- [Anthropic API Docs](https://docs.anthropic.com/en/api/messages)
