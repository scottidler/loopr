//! LLM Client Layer - Anthropic API integration with streaming and tool parsing
//!
//! This module provides:
//! - Message types for LLM communication
//! - LlmClient trait for API abstraction
//! - AnthropicClient implementation (to be added)
//! - Streaming support
//! - Tool call parsing

pub mod client;
pub mod streaming;
pub mod tool_parser;
pub mod types;

// TODO: Add this module in future iteration
// pub mod anthropic;

pub use client::{LlmClient, MockLlmClient};
pub use streaming::{
    create_stream_channel, parse_sse_event, StreamChunk, StreamEvent, StreamHandle, StreamParser,
};
pub use tool_parser::{
    extract_tool_calls, find_tool_definition, needs_tool_execution, parse_response,
    validate_tool_calls, validate_tool_input,
};
pub use types::{
    CompletionRequest, CompletionResponse, Message, Role, StopReason, ToolCall, ToolDefinition,
    ToolResult, Usage,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports() {
        // Verify all public types are accessible
        let _role = Role::User;
        let _stop = StopReason::EndTurn;
    }
}
