//! Anthropic Claude API client implementation

#![allow(dead_code)]

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

use super::client::{
    CompletionRequest, CompletionResponse, LlmClient, LlmError, StopReason, StreamChunk, TokenUsage, ToolCall,
};
use super::tools::ToolDefinition;

/// Configuration for the Anthropic client
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub model: String,
    pub api_key_env: String,
    pub base_url: String,
    pub max_tokens: u32,
    pub timeout_ms: u64,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4-5-20250514".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            max_tokens: 16384,
            timeout_ms: 300_000,
        }
    }
}

/// Anthropic Claude API client
pub struct AnthropicClient {
    model: String,
    api_key: String,
    base_url: String,
    http: Client,
    max_tokens: u32,
}

impl AnthropicClient {
    /// Create a new AnthropicClient from configuration
    pub fn from_config(config: &AnthropicConfig) -> Result<Self, LlmError> {
        let api_key = std::env::var(&config.api_key_env).map_err(|_| LlmError::MissingApiKey {
            env_var: config.api_key_env.clone(),
        })?;

        let http = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(LlmError::Network)?;

        Ok(Self {
            model: config.model.clone(),
            api_key,
            base_url: config.base_url.clone(),
            http,
            max_tokens: config.max_tokens,
        })
    }

    /// Create with explicit values (useful for testing)
    pub fn new(
        model: String,
        api_key: String,
        base_url: String,
        timeout: Duration,
        max_tokens: u32,
    ) -> Result<Self, LlmError> {
        let http = Client::builder().timeout(timeout).build().map_err(LlmError::Network)?;

        Ok(Self {
            model,
            api_key,
            base_url,
            http,
            max_tokens,
        })
    }

    fn build_request_body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens.min(self.max_tokens),
            "system": request.system_prompt,
            "messages": request.messages,
        });

        // Only include tools if there are any
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(
                request
                    .tools
                    .iter()
                    .map(|t| t.to_anthropic_schema())
                    .collect::<Vec<_>>()
            );
        }

        body
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = self.build_request_body(&request);

        let response = self
            .http
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
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::ApiError {
                status: status.as_u16(),
                message: text,
            });
        }

        let api_response: AnthropicResponse = response.json().await?;
        Ok(api_response.into())
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse, LlmError> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);

        let request_builder = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        let mut es = EventSource::new(request_builder).map_err(|e| LlmError::EventSource(e.to_string()))?;

        let mut full_content = String::new();
        let mut tool_calls = Vec::new();
        let mut current_tool: Option<(String, String, String)> = None; // (id, name, json)
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = TokenUsage::default();

        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Message(msg)) => {
                    // Skip [DONE] messages
                    if msg.data == "[DONE]" {
                        break;
                    }

                    let data: serde_json::Value = serde_json::from_str(&msg.data)?;

                    match data["type"].as_str() {
                        Some("content_block_start") => {
                            if let Some(block) = data.get("content_block")
                                && block["type"] == "tool_use"
                            {
                                let id = block["id"].as_str().unwrap_or("").to_string();
                                let name = block["name"].as_str().unwrap_or("").to_string();
                                current_tool = Some((id.clone(), name.clone(), String::new()));
                                let _ = chunk_tx.send(StreamChunk::ToolUseStart { id, name }).await;
                            }
                        }
                        Some("content_block_delta") => {
                            if let Some(delta) = data.get("delta") {
                                if let Some(text) = delta["text"].as_str() {
                                    full_content.push_str(text);
                                    let _ = chunk_tx.send(StreamChunk::TextDelta(text.to_string())).await;
                                }
                                if let Some(json) = delta["partial_json"].as_str()
                                    && let Some((ref id, _, ref mut acc)) = current_tool
                                {
                                    acc.push_str(json);
                                    let _ = chunk_tx
                                        .send(StreamChunk::ToolUseDelta {
                                            id: id.clone(),
                                            json_delta: json.to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                        Some("content_block_stop") => {
                            if let Some((id, name, json)) = current_tool.take() {
                                let input: serde_json::Value =
                                    serde_json::from_str(&json).unwrap_or(serde_json::json!({}));
                                tool_calls.push(ToolCall {
                                    id: id.clone(),
                                    name,
                                    input,
                                });
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
                        Some("error") => {
                            let error_msg = data["error"]["message"].as_str().unwrap_or("Unknown streaming error");
                            let _ = chunk_tx.send(StreamChunk::Error(error_msg.to_string())).await;
                            return Err(LlmError::InvalidResponse(error_msg.to_string()));
                        }
                        _ => {}
                    }
                }
                Ok(Event::Open) => {
                    // Connection opened, continue
                }
                Err(e) => {
                    let _ = chunk_tx.send(StreamChunk::Error(e.to_string())).await;
                    return Err(LlmError::EventSource(e.to_string()));
                }
            }
        }

        let _ = chunk_tx
            .send(StreamChunk::MessageDone {
                stop_reason: stop_reason.clone(),
                usage: usage.clone(),
            })
            .await;

        Ok(CompletionResponse {
            content: if full_content.is_empty() { None } else { Some(full_content) },
            tool_calls,
            stop_reason,
            usage,
        })
    }
}

/// Raw response from Anthropic API
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    stop_reason: String,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

impl From<AnthropicResponse> for CompletionResponse {
    fn from(resp: AnthropicResponse) -> Self {
        let mut text_content = String::new();
        let mut tool_calls = Vec::new();

        for block in resp.content {
            match block {
                AnthropicContentBlock::Text { text } => {
                    text_content.push_str(&text);
                }
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall { id, name, input });
                }
            }
        }

        let stop_reason = match resp.stop_reason.as_str() {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        };

        CompletionResponse {
            content: if text_content.is_empty() { None } else { Some(text_content) },
            tool_calls,
            stop_reason,
            usage: TokenUsage {
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
                cache_read_tokens: resp.usage.cache_read_input_tokens,
                cache_creation_tokens: resp.usage.cache_creation_input_tokens,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_config_default() {
        let config = AnthropicConfig::default();
        assert!(config.model.contains("opus"));
        assert_eq!(config.api_key_env, "ANTHROPIC_API_KEY");
        assert!(config.base_url.contains("anthropic.com"));
    }

    #[test]
    fn test_anthropic_response_conversion() {
        let resp = AnthropicResponse {
            content: vec![AnthropicContentBlock::Text {
                text: "Hello, world!".to_string(),
            }],
            stop_reason: "end_turn".to_string(),
            usage: AnthropicUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        };

        let completion: CompletionResponse = resp.into();
        assert_eq!(completion.content, Some("Hello, world!".to_string()));
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.input_tokens, 100);
        assert_eq!(completion.usage.output_tokens, 50);
    }

    #[test]
    fn test_anthropic_response_with_tool_use() {
        let resp = AnthropicResponse {
            content: vec![AnthropicContentBlock::ToolUse {
                id: "tool_123".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "test.txt"}),
            }],
            stop_reason: "tool_use".to_string(),
            usage: AnthropicUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        };

        let completion: CompletionResponse = resp.into();
        assert!(completion.content.is_none());
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].name, "read_file");
    }

    #[test]
    fn test_stop_reason_parsing() {
        let cases = vec![
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("unknown", StopReason::EndTurn), // Default
        ];

        for (input, expected) in cases {
            let resp = AnthropicResponse {
                content: vec![],
                stop_reason: input.to_string(),
                usage: AnthropicUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
            };
            let completion: CompletionResponse = resp.into();
            assert_eq!(completion.stop_reason, expected);
        }
    }
}
