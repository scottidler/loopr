//! Streaming support for LLM responses.
//!
//! Provides types for handling streaming responses from the Anthropic API,
//! including stream events, chunks, and handles for managing streaming sessions.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Events received during streaming from the Anthropic API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Start of message
    MessageStart {
        /// Message ID from API
        message_id: Option<String>,
    },
    /// Delta containing text content
    ContentBlockStart {
        /// Index of the content block
        index: u32,
        /// Type of content block (text or tool_use)
        content_type: String,
        /// Tool ID if this is a tool_use block
        tool_id: Option<String>,
        /// Tool name if this is a tool_use block
        tool_name: Option<String>,
    },
    /// Text delta within a content block
    ContentBlockDelta {
        /// Index of the content block
        index: u32,
        /// The text delta (for text blocks)
        text: Option<String>,
        /// Partial JSON input delta (for tool_use blocks)
        partial_json: Option<String>,
    },
    /// End of a content block
    ContentBlockStop {
        /// Index of the content block
        index: u32,
    },
    /// Message delta (stop reason, usage)
    MessageDelta {
        /// Stop reason
        stop_reason: Option<String>,
        /// Output token count
        output_tokens: Option<u64>,
    },
    /// Message complete
    MessageStop,
    /// Ping event (keep-alive)
    Ping,
    /// Error event
    Error {
        /// Error message
        message: String,
        /// Error code
        code: Option<String>,
    },
}

/// Chunk types emitted to consumers during streaming.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamChunk {
    /// Text content delta
    Text(String),
    /// Tool call started
    ToolCall {
        /// Unique tool call ID
        id: String,
        /// Name of the tool being called
        name: String,
    },
    /// Tool input JSON delta
    ToolInput {
        /// Tool call ID this input belongs to
        id: String,
        /// Partial JSON input
        input_delta: String,
    },
    /// Stream completed successfully
    Done,
    /// Stream error
    Error(String),
}

/// Handle for receiving streaming chunks.
pub struct StreamHandle {
    /// Receiver for stream chunks
    pub receiver: mpsc::Receiver<StreamChunk>,
}

impl StreamHandle {
    /// Create a new stream handle with the given receiver.
    pub fn new(receiver: mpsc::Receiver<StreamChunk>) -> Self {
        Self { receiver }
    }

    /// Receive the next chunk from the stream.
    pub async fn recv(&mut self) -> Option<StreamChunk> {
        self.receiver.recv().await
    }

    /// Collect all text from the stream into a single string.
    pub async fn collect_text(&mut self) -> String {
        let mut text = String::new();
        while let Some(chunk) = self.recv().await {
            match chunk {
                StreamChunk::Text(t) => text.push_str(&t),
                StreamChunk::Done | StreamChunk::Error(_) => break,
                _ => {}
            }
        }
        text
    }
}

/// Builder for stream handle pairs (sender and handle).
pub fn create_stream_channel(buffer_size: usize) -> (mpsc::Sender<StreamChunk>, StreamHandle) {
    let (tx, rx) = mpsc::channel(buffer_size);
    (tx, StreamHandle::new(rx))
}

/// Parse a raw SSE event line into a StreamEvent.
///
/// Anthropic API uses Server-Sent Events (SSE) format:
/// ```text
/// event: message_start
/// data: {"type": "message_start", ...}
/// ```
pub fn parse_sse_event(data: &str) -> Option<StreamEvent> {
    // Skip empty lines and non-data lines
    if data.is_empty() || data == "[DONE]" {
        return None;
    }

    // Try to parse as JSON
    serde_json::from_str(data).ok()
}

/// State tracker for parsing streaming responses.
#[derive(Debug, Default)]
pub struct StreamParser {
    /// Currently active tool call ID
    pub current_tool_id: Option<String>,
    /// Currently active tool name
    pub current_tool_name: Option<String>,
    /// Accumulated text content
    pub text_content: String,
    /// Accumulated tool input JSON
    pub tool_input: String,
    /// Current content block index
    pub current_index: Option<u32>,
}

impl StreamParser {
    /// Create a new stream parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a stream event and emit chunks.
    pub fn process_event(&mut self, event: StreamEvent) -> Vec<StreamChunk> {
        let mut chunks = Vec::new();

        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_type,
                tool_id,
                tool_name,
            } => {
                self.current_index = Some(index);
                if content_type == "tool_use"
                    && let (Some(id), Some(name)) = (tool_id, tool_name)
                {
                    self.current_tool_id = Some(id.clone());
                    self.current_tool_name = Some(name.clone());
                    self.tool_input.clear();
                    chunks.push(StreamChunk::ToolCall { id, name });
                }
            }
            StreamEvent::ContentBlockDelta {
                index: _,
                text,
                partial_json,
            } => {
                if let Some(t) = text {
                    self.text_content.push_str(&t);
                    chunks.push(StreamChunk::Text(t));
                }
                if let Some(json) = partial_json {
                    self.tool_input.push_str(&json);
                    if let Some(id) = &self.current_tool_id {
                        chunks.push(StreamChunk::ToolInput {
                            id: id.clone(),
                            input_delta: json,
                        });
                    }
                }
            }
            StreamEvent::ContentBlockStop { index: _ } => {
                self.current_tool_id = None;
                self.current_tool_name = None;
                self.current_index = None;
            }
            StreamEvent::MessageStop => {
                chunks.push(StreamChunk::Done);
            }
            StreamEvent::Error { message, code: _ } => {
                chunks.push(StreamChunk::Error(message));
            }
            _ => {}
        }

        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_event_text_delta() {
        let event = StreamEvent::ContentBlockDelta {
            index: 0,
            text: Some("Hello".to_string()),
            partial_json: None,
        };

        let mut parser = StreamParser::new();
        let chunks = parser.process_event(event);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], StreamChunk::Text("Hello".to_string()));
        assert_eq!(parser.text_content, "Hello");
    }

    #[test]
    fn test_stream_event_tool_start() {
        let event = StreamEvent::ContentBlockStart {
            index: 0,
            content_type: "tool_use".to_string(),
            tool_id: Some("tool_123".to_string()),
            tool_name: Some("read_file".to_string()),
        };

        let mut parser = StreamParser::new();
        let chunks = parser.process_event(event);

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            StreamChunk::ToolCall {
                id: "tool_123".to_string(),
                name: "read_file".to_string(),
            }
        );
        assert_eq!(parser.current_tool_id, Some("tool_123".to_string()));
    }

    #[test]
    fn test_stream_event_tool_input() {
        let mut parser = StreamParser::new();

        // First start the tool
        let start_event = StreamEvent::ContentBlockStart {
            index: 0,
            content_type: "tool_use".to_string(),
            tool_id: Some("tool_123".to_string()),
            tool_name: Some("read_file".to_string()),
        };
        parser.process_event(start_event);

        // Then receive input delta
        let delta_event = StreamEvent::ContentBlockDelta {
            index: 0,
            text: None,
            partial_json: Some(r#"{"path":"#.to_string()),
        };
        let chunks = parser.process_event(delta_event);

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            StreamChunk::ToolInput {
                id: "tool_123".to_string(),
                input_delta: r#"{"path":"#.to_string(),
            }
        );
    }

    #[test]
    fn test_stream_event_message_stop() {
        let event = StreamEvent::MessageStop;

        let mut parser = StreamParser::new();
        let chunks = parser.process_event(event);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], StreamChunk::Done);
    }

    #[test]
    fn test_stream_event_error() {
        let event = StreamEvent::Error {
            message: "Rate limited".to_string(),
            code: Some("rate_limit".to_string()),
        };

        let mut parser = StreamParser::new();
        let chunks = parser.process_event(event);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], StreamChunk::Error("Rate limited".to_string()));
    }

    #[test]
    fn test_stream_parser_accumulates_text() {
        let mut parser = StreamParser::new();

        let events = vec![
            StreamEvent::ContentBlockDelta {
                index: 0,
                text: Some("Hello ".to_string()),
                partial_json: None,
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                text: Some("World".to_string()),
                partial_json: None,
            },
        ];

        for event in events {
            parser.process_event(event);
        }

        assert_eq!(parser.text_content, "Hello World");
    }

    #[test]
    fn test_stream_parser_clears_tool_on_stop() {
        let mut parser = StreamParser::new();

        // Start tool
        let start_event = StreamEvent::ContentBlockStart {
            index: 0,
            content_type: "tool_use".to_string(),
            tool_id: Some("tool_123".to_string()),
            tool_name: Some("read_file".to_string()),
        };
        parser.process_event(start_event);

        assert!(parser.current_tool_id.is_some());

        // Stop content block
        let stop_event = StreamEvent::ContentBlockStop { index: 0 };
        parser.process_event(stop_event);

        assert!(parser.current_tool_id.is_none());
        assert!(parser.current_tool_name.is_none());
    }

    #[test]
    fn test_create_stream_channel() {
        let (tx, handle) = create_stream_channel(10);
        drop(tx);
        assert!(handle.receiver.is_closed());
    }

    #[test]
    fn test_parse_sse_event_valid() {
        let json = r#"{"type": "message_stop"}"#;
        let event = parse_sse_event(json);
        assert_eq!(event, Some(StreamEvent::MessageStop));
    }

    #[test]
    fn test_parse_sse_event_invalid() {
        let event = parse_sse_event("not json");
        assert!(event.is_none());
    }

    #[test]
    fn test_parse_sse_event_empty() {
        let event = parse_sse_event("");
        assert!(event.is_none());
    }

    #[test]
    fn test_parse_sse_event_done() {
        let event = parse_sse_event("[DONE]");
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn test_stream_handle_recv() {
        let (tx, mut handle) = create_stream_channel(10);

        tx.send(StreamChunk::Text("Hello".to_string())).await.unwrap();
        tx.send(StreamChunk::Done).await.unwrap();
        drop(tx);

        let chunk1 = handle.recv().await;
        assert_eq!(chunk1, Some(StreamChunk::Text("Hello".to_string())));

        let chunk2 = handle.recv().await;
        assert_eq!(chunk2, Some(StreamChunk::Done));
    }

    #[tokio::test]
    async fn test_stream_handle_collect_text() {
        let (tx, mut handle) = create_stream_channel(10);

        tx.send(StreamChunk::Text("Hello ".to_string())).await.unwrap();
        tx.send(StreamChunk::Text("World".to_string())).await.unwrap();
        tx.send(StreamChunk::Done).await.unwrap();
        drop(tx);

        let text = handle.collect_text().await;
        assert_eq!(text, "Hello World");
    }

    #[tokio::test]
    async fn test_stream_handle_collect_text_ignores_tool_chunks() {
        let (tx, mut handle) = create_stream_channel(10);

        tx.send(StreamChunk::Text("Hello".to_string())).await.unwrap();
        tx.send(StreamChunk::ToolCall {
            id: "123".to_string(),
            name: "test".to_string(),
        })
        .await
        .unwrap();
        tx.send(StreamChunk::Text(" World".to_string())).await.unwrap();
        tx.send(StreamChunk::Done).await.unwrap();
        drop(tx);

        let text = handle.collect_text().await;
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn test_stream_event_serialization() {
        let event = StreamEvent::ContentBlockDelta {
            index: 0,
            text: Some("test".to_string()),
            partial_json: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("content_block_delta"));
        assert!(json.contains("test"));
    }

    #[test]
    fn test_stream_chunk_equality() {
        let chunk1 = StreamChunk::Text("hello".to_string());
        let chunk2 = StreamChunk::Text("hello".to_string());
        let chunk3 = StreamChunk::Text("world".to_string());

        assert_eq!(chunk1, chunk2);
        assert_ne!(chunk1, chunk3);
    }
}
