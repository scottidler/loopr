use eyre::{Result, eyre};
use reqwest::Client;
use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, info, instrument, trace, warn};

use crate::agents::implementer::{ChatMessage, LlmClient};
use crate::agents::{AgentEvent, AgentStatus};
use crate::config::AgentRoleConfig;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::agentic_loop::AgenticLlm;
use crate::tools::types::{ContentBlock, Message, StopReason, ToolDefinition};

/// Real LLM client using reqwest async HTTP with SSE streaming for the Anthropic Messages API.
///
/// Implements `LlmClient` for use by agents. Streams tokens through the broadcast channel
/// as `AgentEvent::LlmOutput` events for real-time TUI display.
pub struct AgentLlmClient {
    client: Client,
    config: AgentRoleConfig,
    api_key: String,
    session_id: String,
    event_tx: broadcast::Sender<DaemonEvent>,
    /// When set, each request/response pair is appended to `{conversation_log_dir}/{conversation_log_name}.log`.
    /// Only set when the session's `conversations/` directory exists (log level != INFO).
    conversation_log_dir: Option<PathBuf>,
    /// File name stem for the conversation log (e.g. "implement-wk-abc12").
    conversation_log_name: String,
}

impl std::fmt::Debug for AgentLlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLlmClient")
            .field("session_id", &self.session_id)
            .field("model", &self.config.model)
            .finish_non_exhaustive()
    }
}

impl AgentLlmClient {
    /// Create a new AgentLlmClient.
    ///
    /// Reads the API key from the environment variable specified in `config.api_key_env`.
    pub fn new(config: AgentRoleConfig, session_id: String, event_tx: broadcast::Sender<DaemonEvent>) -> Result<Self> {
        let api_key = std::env::var(&config.api_key_env)
            .map_err(|_| eyre!("API key not found in env var: {}", config.api_key_env))?;

        let client = Client::new();

        Ok(Self {
            client,
            config,
            api_key,
            session_id,
            event_tx,
            conversation_log_dir: None,
            conversation_log_name: String::new(),
        })
    }

    /// Enable LLM conversation capture for this client.
    ///
    /// `dir` - the `conversations/` directory for this session.
    /// `name` - the log file stem (e.g. `"implement-wk-abc12"`).
    pub fn with_conversation_log(mut self, dir: PathBuf, name: impl Into<String>) -> Self {
        self.conversation_log_dir = Some(dir);
        self.conversation_log_name = name.into();
        self
    }

    /// Create with an explicit API key (used in tests).
    #[allow(clippy::unwrap_used)]
    #[cfg(test)]
    pub fn with_api_key(
        config: AgentRoleConfig,
        session_id: String,
        event_tx: broadcast::Sender<DaemonEvent>,
        api_key: String,
    ) -> Self {
        Self {
            client: Client::new(),
            config,
            api_key,
            session_id,
            event_tx,
            conversation_log_dir: None,
            conversation_log_name: String::new(),
        }
    }

    /// Emit an LlmOutput event through the broadcast channel.
    fn emit_chunk(&self, chunk: &str, is_final: bool) {
        let event = DaemonEvent::new(
            "agent.llm_output",
            serde_json::json!(AgentEvent::LlmOutput {
                session_id: self.session_id.clone(),
                chunk: chunk.to_string(),
                is_final,
            }),
        );
        // Ignore send errors — no subscribers is fine
        let _ = self.event_tx.send(event);
    }

    /// Emit a status change event.
    fn emit_status(&self, status: AgentStatus) {
        tracing::debug!("[agent_status] {}: -> {:?}", self.session_id, status);
        let _ = self
            .event_tx
            .send(DaemonEvent::agent_status_changed(&self.session_id, status));
    }

    /// Append content to the conversation log file, if conversation capture is active.
    fn append_to_conversation_log(&self, content: &str) {
        if let Some(ref dir) = self.conversation_log_dir {
            let path = dir.join(format!("{}.log", self.conversation_log_name));
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = file.write_all(content.as_bytes());
            }
        }
    }

    /// Call the Anthropic Messages API with SSE streaming and multi-turn message history.
    /// Returns the accumulated full response text.
    #[instrument(skip_all)]
    async fn call_streaming_with_messages(&self, system_prompt: &str, messages: &[ChatMessage]) -> Result<String> {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })
            })
            .collect();

        let body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            "stream": true,
            "system": system_prompt,
            "messages": api_messages,
        });

        // Conversation capture: log request before HTTP call.
        if self.conversation_log_dir.is_some() {
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
            let mut entry = format!("=== REQUEST {} ===\n[system]\n{}\n\n", ts, system_prompt);
            for m in messages {
                entry.push_str(&format!("[{}]\n{}\n\n", m.role, m.content));
            }
            self.append_to_conversation_log(&entry);
        }

        self.emit_status(AgentStatus::WaitingForLlm);

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("HTTP request failed: {}", e))?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            warn!("Rate limited (429) — backing off");
            return Err(eyre!("rate limited (429) — retry with backoff"));
        }
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre!("API error {}: {}", status, error_body));
        }

        self.emit_status(AgentStatus::Running);

        // Read SSE stream
        let full_text = self.read_sse_stream(response).await?;

        // Conversation capture: log response.
        if self.conversation_log_dir.is_some() {
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
            let entry = format!("=== RESPONSE {} ===\n{}\n\n", ts, full_text);
            self.append_to_conversation_log(&entry);
        }

        // Emit final chunk marker
        self.emit_chunk("", true);

        Ok(full_text)
    }

    /// Call the Anthropic Messages API with SSE streaming (single user message).
    /// Delegates to `call_streaming_with_messages`.
    #[instrument(skip_all)]
    async fn call_streaming(&self, system_prompt: &str, user_message: &str) -> Result<String> {
        self.call_streaming_with_messages(system_prompt, &[ChatMessage::user(user_message)])
            .await
    }

    /// Read an SSE stream from the response and accumulate text.
    #[instrument(skip_all)]
    async fn read_sse_stream(&self, response: reqwest::Response) -> Result<String> {
        use futures::StreamExt;

        let mut accumulated = String::new();
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| eyre!("stream read error: {}", e))?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            buffer.push_str(&chunk_str);

            // Process complete SSE lines from buffer
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim_end_matches('\r').to_string();
                buffer = buffer[line_end + 1..].to_string();

                if let Some(text) = parse_sse_text_delta(&line) {
                    accumulated.push_str(&text);
                    self.emit_chunk(&text, false);
                    trace!("LLM chunk: {} bytes", text.len());
                }
            }
        }

        // Process any remaining data in buffer
        if !buffer.is_empty() {
            for line in buffer.lines() {
                if let Some(text) = parse_sse_text_delta(line) {
                    accumulated.push_str(&text);
                    self.emit_chunk(&text, false);
                }
            }
        }

        info!(
            "LLM response complete: {} chars for session {}",
            accumulated.len(),
            self.session_id
        );
        Ok(accumulated)
    }
}

impl LlmClient for AgentLlmClient {
    fn call<'a>(
        &'a self,
        system_prompt: &'a str,
        user_message: &'a str,
    ) -> impl Future<Output = Result<String>> + Send + 'a {
        async move {
            debug!(
                "AgentLlmClient::call(system_prompt_len={}, user_msg_len={})",
                system_prompt.len(),
                user_message.len()
            );
            self.call_streaming(system_prompt, user_message).await
        }
    }

    fn call_with_history<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<String>> + Send + 'a {
        async move {
            debug!(
                "AgentLlmClient::call_with_history(system_prompt_len={}, messages={})",
                system_prompt.len(),
                messages.len()
            );
            self.call_streaming_with_messages(system_prompt, messages).await
        }
    }
}

impl AgenticLlm for AgentLlmClient {
    /// Call the Anthropic Messages API with SSE streaming and tool support.
    ///
    /// Text chunks are emitted live via `emit_chunk` for real-time TUI display.
    /// Tool input JSON is buffered silently until `content_block_stop`, then assembled
    /// into complete `ContentBlock::ToolUse` entries. Returns the same atomic
    /// `(Vec<ContentBlock>, Option<StopReason>)` that `run_tool_loop` expects.
    #[instrument(skip_all, fields(message_count = %messages.len(), tool_count = %tools.len()))]
    async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<(Vec<ContentBlock>, Option<StopReason>)> {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let content: Vec<serde_json::Value> = m
                    .content
                    .iter()
                    .map(|block| serde_json::to_value(block).unwrap_or_default())
                    .collect();
                serde_json::json!({
                    "role": m.role,
                    "content": content,
                })
            })
            .collect();

        let tool_defs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let system_block = serde_json::json!([
            {
                "type": "text",
                "text": system_prompt,
                "cache_control": {"type": "ephemeral"}
            }
        ]);

        let mut body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            "stream": true,
            "system": system_block,
            "messages": api_messages,
        });

        if !tool_defs.is_empty() {
            // Add cache_control to the last tool definition for prompt caching
            let mut tool_array = tool_defs;
            if let Some(last) = tool_array.last_mut() {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
            body["tools"] = serde_json::Value::Array(tool_array);
        }

        // Conversation capture: log request before HTTP call.
        if self.conversation_log_dir.is_some() {
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
            let mut entry = format!("=== REQUEST {} ===\n[system]\n{}\n\n", ts, system_prompt);
            for m in messages {
                let content_text: String = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                entry.push_str(&format!("[{}]\n{}\n\n", m.role, content_text));
            }
            if !tools.is_empty() {
                entry.push_str(&format!(
                    "[tools]\n{}\n\n",
                    serde_json::to_string(tools).unwrap_or_default()
                ));
            }
            self.append_to_conversation_log(&entry);
        }

        self.emit_status(AgentStatus::WaitingForLlm);

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("HTTP request failed: {}", e))?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            warn!("Rate limited (429) — backing off");
            return Err(eyre!("rate limited (429) — retry with backoff"));
        }
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            return Err(eyre!("API error {}: {}", status, error_body));
        }

        self.emit_status(AgentStatus::Running);

        let call_start = Instant::now();

        // Parse SSE stream into content blocks
        let (content_blocks, stop_reason, ttft) = self.read_sse_content_blocks(response).await?;

        let total_ms = call_start.elapsed().as_millis();
        let ttft_ms = ttft.map(|d| d.as_millis()).unwrap_or(0);
        debug!(
            "[timing:{}] complete: total={}ms ttft={}ms blocks={}",
            self.session_id,
            total_ms,
            ttft_ms,
            content_blocks.len()
        );

        // Conversation capture: log response content blocks.
        if self.conversation_log_dir.is_some() {
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
            let mut entry = format!("=== RESPONSE {} ===\n", ts);
            for block in &content_blocks {
                match block {
                    ContentBlock::Text { text } => entry.push_str(text),
                    other => entry.push_str(&serde_json::to_string(other).unwrap_or_default()),
                }
                entry.push('\n');
            }
            entry.push('\n');
            self.append_to_conversation_log(&entry);
        }

        Ok((content_blocks, stop_reason))
    }
}

/// Per-block accumulator used while reading the SSE stream.
enum BlockAccumulator {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

impl AgentLlmClient {
    /// Read an SSE stream and assemble it into typed `ContentBlock`s.
    ///
    /// - Text deltas are emitted live via `emit_chunk` and accumulated into `ContentBlock::Text`.
    /// - Tool input_json deltas are buffered silently, parsed on `content_block_stop`,
    ///   and assembled into `ContentBlock::ToolUse`.
    /// - `message_delta` with `stop_reason` is captured for the return value.
    #[instrument(skip_all)]
    async fn read_sse_content_blocks(
        &self,
        response: reqwest::Response,
    ) -> Result<(Vec<ContentBlock>, Option<StopReason>, Option<Duration>)> {
        use futures::StreamExt;

        let stream_start = Instant::now();
        let mut first_chunk_time: Option<Duration> = None;
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut current: Option<BlockAccumulator> = None;
        let mut stop_reason: Option<StopReason> = None;

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| eyre!("stream read error: {}", e))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim_end_matches('\r').to_string();
                buffer = buffer[line_end + 1..].to_string();

                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };

                let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match event_type {
                    "content_block_start" => {
                        // Finalize any previous block
                        if let Some(acc) = current.take() {
                            blocks.push(finalize_block(acc));
                        }
                        let cb = &value["content_block"];
                        let block_type = cb.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        current = Some(match block_type {
                            "tool_use" => BlockAccumulator::ToolUse {
                                id: cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                name: cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                input_json: String::new(),
                            },
                            _ => BlockAccumulator::Text(String::new()),
                        });
                    }
                    "content_block_delta" => {
                        if first_chunk_time.is_none() {
                            first_chunk_time = Some(stream_start.elapsed());
                        }
                        let delta = &value["delta"];
                        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match (&mut current, delta_type) {
                            (Some(BlockAccumulator::Text(buf)), "text_delta") => {
                                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                    buf.push_str(text);
                                    self.emit_chunk(text, false);
                                }
                            }
                            (Some(BlockAccumulator::ToolUse { input_json, .. }), "input_json_delta") => {
                                if let Some(json_chunk) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                    input_json.push_str(json_chunk);
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        if let Some(acc) = current.take() {
                            blocks.push(finalize_block(acc));
                        }
                    }
                    "message_delta" => {
                        if let Some(reason_str) = value
                            .get("delta")
                            .and_then(|d| d.get("stop_reason"))
                            .and_then(|v| v.as_str())
                        {
                            stop_reason = Some(match reason_str {
                                "tool_use" => StopReason::ToolUse,
                                "max_tokens" => StopReason::MaxTokens,
                                "stop_sequence" => StopReason::StopSequence,
                                _ => StopReason::EndTurn,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Finalize any trailing block
        if let Some(acc) = current.take() {
            blocks.push(finalize_block(acc));
        }

        Ok((blocks, stop_reason, first_chunk_time))
    }
}

/// Convert a block accumulator into a typed ContentBlock.
fn finalize_block(acc: BlockAccumulator) -> ContentBlock {
    match acc {
        BlockAccumulator::Text(text) => ContentBlock::Text { text },
        BlockAccumulator::ToolUse { id, name, input_json } => {
            let input = serde_json::from_str(&input_json).unwrap_or(serde_json::json!({}));
            ContentBlock::ToolUse { id, name, input }
        }
    }
}

/// Parse an SSE data line for a content_block_delta text chunk.
///
/// Anthropic SSE format:
/// ```text
/// event: content_block_delta
/// data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
/// ```
fn parse_sse_text_delta(line: &str) -> Option<String> {
    let data = line.strip_prefix("data: ")?;

    // Skip non-JSON lines
    if data.is_empty() || data == "[DONE]" {
        return None;
    }

    let value: serde_json::Value = serde_json::from_str(data).ok()?;

    // Only extract text from content_block_delta events
    if value.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }

    let text = value.get("delta")?.get("text")?.as_str()?.to_string();

    Some(text)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // --- SSE parsing tests ---

    #[test]
    fn test_parse_sse_text_delta_valid() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn test_parse_sse_text_delta_multiword() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, Some(" world".to_string()));
    }

    #[test]
    fn test_parse_sse_text_delta_message_start() {
        let line = r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[]}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_message_stop() {
        let line = r#"data: {"type":"message_stop"}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_content_block_start() {
        let line = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_content_block_stop() {
        let line = r#"data: {"type":"content_block_stop","index":0}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_done() {
        let line = "data: [DONE]";
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_not_data_line() {
        let line = "event: content_block_delta";
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_empty_data() {
        let line = "data: ";
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_invalid_json() {
        let line = "data: not json at all";
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_text_delta_empty_text() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn test_parse_sse_text_delta_special_chars() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fn main() {\n    println!(\"hello\");\n}"}}"#;
        let result = parse_sse_text_delta(line);
        assert!(result.is_some());
        assert!(result.unwrap().contains("fn main()"));
    }

    #[test]
    fn test_parse_sse_text_delta_message_delta() {
        let line = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    // --- AgentLlmClient construction tests ---

    #[test]
    fn test_agent_llm_client_with_api_key() {
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(
            config.clone(),
            "session-1".to_string(),
            event_tx,
            "test-key".to_string(),
        );
        assert_eq!(client.session_id, "session-1");
        assert_eq!(client.api_key, "test-key");
        assert_eq!(client.config.model, config.model);
    }

    #[test]
    fn test_agent_llm_client_new_missing_env() {
        let mut config = AgentRoleConfig::default_implementer();
        config.api_key_env = "LOOPR_TEST_NONEXISTENT_KEY_12345".to_string();
        let (event_tx, _) = broadcast::channel(16);
        let result = AgentLlmClient::new(config, "s1".to_string(), event_tx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not found"));
    }

    #[test]
    fn test_agent_llm_client_emit_chunk() {
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string());

        client.emit_chunk("Hello", false);

        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.event, "agent.llm_output");
        let agent_event: AgentEvent = serde_json::from_value(event.data).unwrap();
        if let AgentEvent::LlmOutput {
            session_id,
            chunk,
            is_final,
        } = agent_event
        {
            assert_eq!(session_id, "s1");
            assert_eq!(chunk, "Hello");
            assert!(!is_final);
        } else {
            panic!("expected LlmOutput event");
        }
    }

    #[test]
    fn test_agent_llm_client_emit_final_chunk() {
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string());

        client.emit_chunk("", true);

        let event = event_rx.try_recv().unwrap();
        let agent_event: AgentEvent = serde_json::from_value(event.data).unwrap();
        if let AgentEvent::LlmOutput { is_final, .. } = agent_event {
            assert!(is_final);
        } else {
            panic!("expected LlmOutput event");
        }
    }

    #[test]
    fn test_agent_llm_client_emit_status() {
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string());

        client.emit_status(AgentStatus::WaitingForLlm);

        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.event, "agent.status_changed");
    }

    // --- Streaming accumulation tests ---

    /// Simulate SSE lines and verify accumulated text.
    #[test]
    fn test_accumulate_sse_chunks() {
        let sse_lines = vec![
            r#"event: message_start"#,
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[]}}"#,
            "",
            r#"event: content_block_start"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "",
            r#"event: content_block_delta"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            "",
            r#"event: content_block_delta"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            "",
            r#"event: content_block_stop"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            r#"event: message_delta"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
            "",
            r#"event: message_stop"#,
            r#"data: {"type":"message_stop"}"#,
        ];

        let mut accumulated = String::new();
        for line in &sse_lines {
            if let Some(text) = parse_sse_text_delta(line) {
                accumulated.push_str(&text);
            }
        }
        assert_eq!(accumulated, "Hello world");
    }

    /// Full SSE stream with JSON action output.
    #[test]
    fn test_accumulate_sse_json_response() {
        let lines = vec![
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"[{\"action\""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":": \"done\", \"summary\""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":": \"All complete\"}]"}}"#,
        ];

        let mut accumulated = String::new();
        for line in &lines {
            if let Some(text) = parse_sse_text_delta(line) {
                accumulated.push_str(&text);
            }
        }
        assert_eq!(accumulated, r#"[{"action": "done", "summary": "All complete"}]"#);
    }

    // --- Error path tests ---

    #[test]
    fn test_parse_sse_malformed_json() {
        // Partial/broken JSON should return None (graceful handling)
        let line = "data: {broken";
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_missing_delta_field() {
        // Valid JSON but missing delta.text
        let line = r#"data: {"type":"content_block_delta","index":0}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_missing_type_field() {
        // Valid JSON but no "type" field
        let line = r#"data: {"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_wrong_delta_type() {
        // type is content_block_delta but delta type is not text_delta
        let line =
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_emit_chunk_no_subscribers() {
        // No receiver subscribed — emit_chunk should not panic
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        // Drop the only receiver
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string());

        // This should not panic even with no subscribers
        client.emit_chunk("Hello", false);
        client.emit_chunk("", true);
    }

    #[test]
    fn test_emit_status_no_subscribers() {
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string());

        // Should not panic
        client.emit_status(AgentStatus::Running);
        client.emit_status(AgentStatus::WaitingForLlm);
    }

    #[tokio::test]
    async fn test_call_streaming_network_error() {
        let mut config = AgentRoleConfig::default_implementer();
        config.model = "test-model".to_string();
        let (event_tx, _) = broadcast::channel(16);

        // Use a non-routable address to trigger network error
        let client = AgentLlmClient {
            client: Client::new(),
            config,
            api_key: "test-key".to_string(),
            session_id: "s1".to_string(),
            event_tx,
            conversation_log_dir: None,
            conversation_log_name: String::new(),
        };

        // Override the URL by calling call_streaming which always uses api.anthropic.com
        // We can't easily override the URL, but we can test with an invalid key
        // The call will fail either with network error or auth error
        let result = client.call_streaming("system", "hello").await;
        // Should fail (either network or auth error)
        assert!(result.is_err());
    }

    #[test]
    fn test_agent_llm_client_debug() {
        let config = AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        let client =
            AgentLlmClient::with_api_key(config, "debug-session".to_string(), event_tx, "secret-key".to_string());

        let debug_str = format!("{:?}", client);
        assert!(debug_str.contains("debug-session"));
        // Should NOT contain the API key
        assert!(!debug_str.contains("secret-key"));
    }

    #[test]
    fn test_parse_sse_unicode_text() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello \u00e9\u00e8\u00ea"}}"#;
        let result = parse_sse_text_delta(line);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_sse_large_text_chunk() {
        // Simulate a large text chunk
        let large_text = "x".repeat(10000);
        let line = format!(
            r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{}"}}}}"#,
            large_text
        );
        let result = parse_sse_text_delta(&line);
        assert_eq!(result, Some(large_text));
    }

    #[test]
    fn test_parse_sse_null_text_field() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":null}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_sse_numeric_text_field() {
        // text is a number, not a string
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":42}}"#;
        let result = parse_sse_text_delta(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_accumulate_sse_with_empty_lines() {
        // SSE streams have blank lines between events
        let lines = vec![
            "",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A"}}"#,
            "",
            "",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"B"}}"#,
            "",
        ];
        let mut accumulated = String::new();
        for line in &lines {
            if let Some(text) = parse_sse_text_delta(line) {
                accumulated.push_str(&text);
            }
        }
        assert_eq!(accumulated, "AB");
    }

    #[test]
    fn test_finalize_block_text() {
        let block = finalize_block(BlockAccumulator::Text("hello world".into()));
        assert!(matches!(block, ContentBlock::Text { text } if text == "hello world"));
    }

    #[test]
    fn test_finalize_block_tool_use() {
        let block = finalize_block(BlockAccumulator::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input_json: r#"{"path":"foo.rs"}"#.into(),
        });
        match block {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "foo.rs");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_finalize_block_tool_use_bad_json() {
        let block = finalize_block(BlockAccumulator::ToolUse {
            id: "call_2".into(),
            name: "shell".into(),
            input_json: "not json".into(),
        });
        match block {
            ContentBlock::ToolUse { input, .. } => {
                assert_eq!(input, serde_json::json!({}));
            }
            _ => panic!("expected ToolUse"),
        }
    }

    // --- conversation log tests ---

    #[test]
    fn test_append_to_conversation_log_writes_when_dir_set() {
        let dir = crate::test_util::TestDir::new("loopr-conv-log-test");
        let config = crate::config::AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string())
            .with_conversation_log(dir.to_path_buf(), "test-session");

        client.append_to_conversation_log("hello world\n");

        let log_path = dir.join("test-session.log");
        assert!(log_path.exists(), "log file should be created");
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(contents, "hello world\n");
    }

    #[test]
    fn test_append_to_conversation_log_noop_when_dir_none() {
        let config = crate::config::AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string());

        // Should not panic and should write nothing
        client.append_to_conversation_log("should not appear");
        // No assertion needed - just verify no panic
    }

    #[test]
    fn test_conversation_log_appends_multiple_calls() {
        let dir = crate::test_util::TestDir::new("loopr-conv-multi-test");
        let config = crate::config::AgentRoleConfig::default_implementer();
        let (event_tx, _) = broadcast::channel(16);
        let client = AgentLlmClient::with_api_key(config, "s1".to_string(), event_tx, "key".to_string())
            .with_conversation_log(dir.to_path_buf(), "multi");

        client.append_to_conversation_log("line 1\n");
        client.append_to_conversation_log("line 2\n");

        let contents = std::fs::read_to_string(dir.join("multi.log")).unwrap();
        assert_eq!(contents, "line 1\nline 2\n");
    }
}
