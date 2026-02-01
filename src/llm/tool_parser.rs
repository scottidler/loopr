//! Tool parser for extracting tool calls from Anthropic API responses
//!
//! This module provides utilities for parsing tool_use content blocks
//! from Anthropic API responses and validating tool call inputs.

use crate::error::{LooprError, Result};
use crate::llm::types::{CompletionResponse, StopReason, ToolCall, ToolDefinition, Usage};
use serde_json::Value;

/// Parse a raw Anthropic API response into a CompletionResponse
///
/// Handles both text and tool_use content blocks from the response.
pub fn parse_response(response: &Value) -> Result<CompletionResponse> {
    let mut content = String::new();
    let mut tool_calls = Vec::new();

    // Parse content blocks
    if let Some(content_blocks) = response.get("content").and_then(|c| c.as_array()) {
        for block in content_blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(text);
                    }
                }
                Some("tool_use") => {
                    if let Some(call) = parse_tool_use_block(block) {
                        tool_calls.push(call);
                    }
                }
                _ => {} // Skip unknown block types
            }
        }
    }

    // Parse stop reason
    let stop_reason = response
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(parse_stop_reason)
        .unwrap_or(StopReason::EndTurn);

    // Parse usage
    let usage = response.get("usage").map(parse_usage).unwrap_or_default();

    Ok(CompletionResponse {
        content,
        tool_calls,
        stop_reason,
        usage,
    })
}

/// Parse a single tool_use content block into a ToolCall
fn parse_tool_use_block(block: &Value) -> Option<ToolCall> {
    let id = block.get("id").and_then(|v| v.as_str())?.to_string();
    let name = block.get("name").and_then(|v| v.as_str())?.to_string();
    let input = block.get("input").cloned().unwrap_or(Value::Object(Default::default()));

    Some(ToolCall { id, name, input })
}

/// Parse stop reason string into StopReason enum
fn parse_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

/// Parse usage object from response
fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    }
}

/// Validate a tool call's input against a tool definition's schema
///
/// Checks that all required fields are present in the input.
pub fn validate_tool_input(call: &ToolCall, definition: &ToolDefinition) -> Result<()> {
    let schema = &definition.input_schema;

    // Check required fields
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for req in required {
            if let Some(field_name) = req.as_str()
                && call.input.get(field_name).is_none()
            {
                return Err(LooprError::ValidationFailed(format!(
                    "Tool '{}' missing required field: {}",
                    call.name, field_name
                )));
            }
        }
    }

    Ok(())
}

/// Find a tool definition by name in a list
pub fn find_tool_definition<'a>(name: &str, tools: &'a [ToolDefinition]) -> Option<&'a ToolDefinition> {
    tools.iter().find(|t| t.name == name)
}

/// Validate all tool calls in a response against available tool definitions
pub fn validate_tool_calls(response: &CompletionResponse, tools: &[ToolDefinition]) -> Result<()> {
    for call in &response.tool_calls {
        // Check tool exists
        let definition = find_tool_definition(&call.name, tools)
            .ok_or_else(|| LooprError::ValidationFailed(format!("Unknown tool: {}", call.name)))?;

        // Validate input
        validate_tool_input(call, definition)?;
    }

    Ok(())
}

/// Extract tool calls from a response, returning empty vec if none
pub fn extract_tool_calls(response: &CompletionResponse) -> Vec<ToolCall> {
    response.tool_calls.clone()
}

/// Check if a response requires tool execution
pub fn needs_tool_execution(response: &CompletionResponse) -> bool {
    !response.tool_calls.is_empty() || response.stop_reason == StopReason::ToolUse
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_response_text_only() {
        let response = json!({
            "content": [
                {"type": "text", "text": "Hello, world!"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let result = parse_response(&response).unwrap();
        assert_eq!(result.content, "Hello, world!");
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 5);
    }

    #[test]
    fn test_parse_response_with_tool_use() {
        let response = json!({
            "content": [
                {"type": "text", "text": "Let me read that file."},
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "read_file",
                    "input": {"path": "/tmp/test.txt"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 15}
        });

        let result = parse_response(&response).unwrap();
        assert_eq!(result.content, "Let me read that file.");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "toolu_123");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(result.tool_calls[0].input["path"], "/tmp/test.txt");
        assert_eq!(result.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn test_parse_response_multiple_tools() {
        let response = json!({
            "content": [
                {"type": "text", "text": "I'll run these commands."},
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "bash",
                    "input": {"command": "ls -la"}
                },
                {
                    "type": "tool_use",
                    "id": "toolu_2",
                    "name": "read_file",
                    "input": {"path": "/etc/hosts"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 30, "output_tokens": 25}
        });

        let result = parse_response(&response).unwrap();
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].name, "bash");
        assert_eq!(result.tool_calls[1].name, "read_file");
    }

    #[test]
    fn test_parse_response_empty_content() {
        let response = json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 0, "output_tokens": 0}
        });

        let result = parse_response(&response).unwrap();
        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn test_parse_response_missing_fields() {
        let response = json!({});
        let result = parse_response(&response).unwrap();
        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(result.usage.total(), 0);
    }

    #[test]
    fn test_parse_stop_reason() {
        assert_eq!(parse_stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(parse_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(parse_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(parse_stop_reason("stop_sequence"), StopReason::StopSequence);
        assert_eq!(parse_stop_reason("unknown"), StopReason::EndTurn);
    }

    #[test]
    fn test_validate_tool_input_valid() {
        let call = ToolCall::new("id1", "read_file", json!({"path": "/tmp/test.txt"}));
        let definition = ToolDefinition::new(
            "read_file",
            "Read file",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        );

        assert!(validate_tool_input(&call, &definition).is_ok());
    }

    #[test]
    fn test_validate_tool_input_missing_required() {
        let call = ToolCall::new("id1", "read_file", json!({}));
        let definition = ToolDefinition::new(
            "read_file",
            "Read file",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        );

        let result = validate_tool_input(&call, &definition);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing required field"));
        assert!(err.contains("path"));
    }

    #[test]
    fn test_validate_tool_input_no_required() {
        let call = ToolCall::new("id1", "simple_tool", json!({}));
        let definition = ToolDefinition::new(
            "simple_tool",
            "A simple tool",
            json!({"type": "object", "properties": {}}),
        );

        assert!(validate_tool_input(&call, &definition).is_ok());
    }

    #[test]
    fn test_find_tool_definition() {
        let tools = vec![
            ToolDefinition::new("read_file", "Read file", json!({})),
            ToolDefinition::new("write_file", "Write file", json!({})),
            ToolDefinition::new("bash", "Run bash", json!({})),
        ];

        assert!(find_tool_definition("read_file", &tools).is_some());
        assert!(find_tool_definition("bash", &tools).is_some());
        assert!(find_tool_definition("unknown", &tools).is_none());
    }

    #[test]
    fn test_validate_tool_calls_valid() {
        let response = CompletionResponse {
            content: String::new(),
            tool_calls: vec![ToolCall::new("id1", "read_file", json!({"path": "/tmp/x"}))],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };
        let tools = vec![ToolDefinition::new(
            "read_file",
            "Read file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        )];

        assert!(validate_tool_calls(&response, &tools).is_ok());
    }

    #[test]
    fn test_validate_tool_calls_unknown_tool() {
        let response = CompletionResponse {
            content: String::new(),
            tool_calls: vec![ToolCall::new("id1", "unknown_tool", json!({}))],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };
        let tools = vec![ToolDefinition::new("read_file", "Read file", json!({}))];

        let result = validate_tool_calls(&response, &tools);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown tool"));
    }

    #[test]
    fn test_extract_tool_calls() {
        let response = CompletionResponse {
            content: String::new(),
            tool_calls: vec![
                ToolCall::new("id1", "tool1", json!({})),
                ToolCall::new("id2", "tool2", json!({})),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };

        let calls = extract_tool_calls(&response);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn test_needs_tool_execution_true() {
        let response = CompletionResponse {
            content: String::new(),
            tool_calls: vec![ToolCall::new("id1", "tool1", json!({}))],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };

        assert!(needs_tool_execution(&response));
    }

    #[test]
    fn test_needs_tool_execution_false() {
        let response = CompletionResponse {
            content: "Hello".to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        };

        assert!(!needs_tool_execution(&response));
    }

    #[test]
    fn test_parse_multiple_text_blocks() {
        let response = json!({
            "content": [
                {"type": "text", "text": "First part."},
                {"type": "text", "text": "Second part."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 10}
        });

        let result = parse_response(&response).unwrap();
        assert_eq!(result.content, "First part.\nSecond part.");
    }

    #[test]
    fn test_parse_tool_use_empty_input() {
        let response = json!({
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "get_time"
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });

        let result = parse_response(&response).unwrap();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "get_time");
        assert!(result.tool_calls[0].input.is_object());
    }
}
