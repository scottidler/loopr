//! LLM Client Layer - Anthropic API integration with streaming and tool parsing
//!
//! This module provides:
//! - Message types for LLM communication
//! - LlmClient trait for API abstraction
//! - AnthropicClient implementation (to be added)
//! - Streaming support
//! - Tool call parsing (to be added)

pub mod client;
pub mod streaming;
pub mod types;

// TODO: Add these modules in future iterations
// pub mod anthropic;
// pub mod tool_parser;

pub use client::{LlmClient, MockLlmClient};
pub use streaming::{StreamChunk, StreamEvent, StreamHandle, StreamParser, create_stream_channel, parse_sse_event};
pub use types::{
    CompletionRequest, CompletionResponse, Message, Role, StopReason, ToolCall, ToolDefinition, ToolResult, Usage,
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
