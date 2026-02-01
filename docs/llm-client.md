# LLM Client

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/llm-client.md

---

## Summary

The LLM client handles all communication with Claude (Anthropic API). It runs in the daemon process, manages streaming responses, handles tool calls, and tracks token usage.

---

## Architecture

```
Daemon
└── LlmClient (trait)
    └── AnthropicClient (implementation)
        ├── chat() - Main conversation
        ├── complete() - Simple completion
        └── stream() - Streaming response
```

---

## LlmClient Trait

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat message and get response with tool calls
    async fn chat(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse>;

    /// Simple completion (no tools)
    async fn complete(&self, prompt: &str) -> Result<String>;

    /// Stream response chunks
    async fn stream(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<LlmResponse>;

    /// Get token usage for session
    fn usage(&self) -> TokenUsage;
}

pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub enum StreamChunk {
    Text(String),
    ToolCall { id: String, name: String },
    ToolInput { id: String, input_delta: String },
    Done,
}

pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}
```

---

## Anthropic Client

```rust
pub struct AnthropicClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    usage: Arc<Mutex<TokenUsage>>,
}

impl AnthropicClient {
    pub fn new(config: &LlmConfig) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| eyre!("ANTHROPIC_API_KEY not set"))?;

        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            usage: Arc::new(Mutex::new(TokenUsage::default())),
        })
    }

    fn build_request(&self, prompt: &str, tools: &[ToolDefinition]) -> Value {
        let mut request = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [{
                "role": "user",
                "content": prompt
            }]
        });

        if !tools.is_empty() {
            request["tools"] = tools.iter()
                .map(|t| t.to_anthropic_schema())
                .collect();
        }

        request
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn chat(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse> {
        let request = self.build_request(prompt, tools);

        let response = self.client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if response.status() == 429 {
            let retry_after = response.headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            return Err(LlmError::RateLimited {
                retry_after: Duration::from_secs(retry_after),
            }.into());
        }

        let body: Value = response.json().await?;
        self.parse_response(body)
    }

    async fn stream(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<LlmResponse> {
        let mut request = self.build_request(prompt, tools);
        request["stream"] = json!(true);

        let response = self.client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        let mut stream = response.bytes_stream();
        let mut full_response = LlmResponse::default();
        let mut current_tool: Option<(String, String)> = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    let _ = tx.send(StreamChunk::Done).await;
                    break;
                }

                let event: Value = serde_json::from_str(data)?;
                self.handle_stream_event(&event, &tx, &mut full_response, &mut current_tool).await?;
            }
        }

        Ok(full_response)
    }

    fn usage(&self) -> TokenUsage {
        self.usage.lock().unwrap().clone()
    }
}
```

---

## Tool Call Handling

When the LLM returns tool calls, the daemon executes them and continues:

```rust
impl LoopManager {
    async fn run_llm_turn(
        &self,
        loop_impl: &mut dyn Loop,
        worktree: &Path,
    ) -> Result<()> {
        let tools = loop_impl.tools();
        let prompt = loop_impl.build_prompt(&self.config)?;

        // Start streaming
        let (tx, mut rx) = mpsc::channel(100);
        let response_future = self.llm_client.stream(&prompt, &tools, tx);

        // Forward chunks to TUI
        while let Some(chunk) = rx.recv().await {
            self.notify_tuis(DaemonEvent::ChatChunk(chunk));
        }

        let response = response_future.await?;

        // Execute tool calls
        if !response.tool_calls.is_empty() {
            let mut tool_results = Vec::new();

            for call in response.tool_calls {
                self.notify_tuis(DaemonEvent::ToolCall(call.clone()));

                let result = self.execute_tool(loop_impl.id(), worktree, call.clone()).await?;

                self.notify_tuis(DaemonEvent::ToolResult(result.clone()));
                tool_results.push((call, result));
            }

            // Continue conversation with tool results
            let continuation = self.build_tool_result_prompt(&tool_results);
            let next_response = self.llm_client.chat(&continuation, &tools).await?;

            // Recursively handle if more tool calls
            if !next_response.tool_calls.is_empty() {
                // ... continue loop
            }
        }

        Ok(())
    }

    fn build_tool_result_prompt(&self, results: &[(ToolCall, ToolResult)]) -> String {
        let mut prompt = String::new();
        for (call, result) in results {
            prompt.push_str(&format!(
                "Tool: {}\nResult: {}\n\n",
                call.name,
                if result.is_error {
                    format!("ERROR: {}", result.content)
                } else {
                    result.content.clone()
                }
            ));
        }
        prompt
    }
}
```

---

## Rate Limiting

```rust
impl AnthropicClient {
    async fn chat_with_retry(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        max_retries: u32,
    ) -> Result<LlmResponse> {
        let mut retries = 0;

        loop {
            match self.chat(prompt, tools).await {
                Ok(response) => return Ok(response),
                Err(e) if e.is::<LlmError>() => {
                    if let Some(LlmError::RateLimited { retry_after }) = e.downcast_ref() {
                        if retries >= max_retries {
                            return Err(e);
                        }
                        retries += 1;
                        tracing::warn!(
                            retry_after_secs = retry_after.as_secs(),
                            retries,
                            "Rate limited, waiting"
                        );
                        tokio::time::sleep(*retry_after).await;
                        continue;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}
```

---

## Configuration

```yaml
# loopr.yml
llm:
  model: "claude-sonnet-4-20250514"
  max_tokens: 8192
  timeout_ms: 300000

  # Different models for different purposes
  models:
    default: "claude-sonnet-4-20250514"
    review: "claude-3-haiku-20240307"  # Cheaper for validation
    complex: "claude-opus-4-5-20250514"  # For difficult tasks
```

---

## Token Tracking

```rust
impl AnthropicClient {
    fn track_usage(&self, response: &Value) {
        if let Some(usage) = response.get("usage") {
            let input = usage["input_tokens"].as_u64().unwrap_or(0);
            let output = usage["output_tokens"].as_u64().unwrap_or(0);

            let mut total = self.usage.lock().unwrap();
            total.input_tokens += input;
            total.output_tokens += output;

            tracing::debug!(input, output, "Token usage");
        }
    }
}

// Cost calculation
impl TokenUsage {
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
```

---

## Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Rate limited, retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },

    #[error("API error: {message}")]
    ApiError { message: String, code: Option<String> },

    #[error("Request timeout")]
    Timeout,

    #[error("Invalid response: {0}")]
    InvalidResponse(String),
}
```

---

## References

- [architecture.md](architecture.md) - System overview
- [tools.md](tools.md) - Tool definitions
- [loop-architecture.md](loop-architecture.md) - How loops use LLM
