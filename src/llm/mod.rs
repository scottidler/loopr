//! LLM Client Layer - Anthropic API integration with streaming and tool parsing
//!
//! This module provides:
//! - Message types for LLM communication
//! - LlmClient trait for API abstraction
//! - AnthropicClient implementation
//! - Streaming support
//! - Tool call parsing

pub mod anthropic;
pub mod client;
pub mod streaming;
pub mod tool_parser;
pub mod types;

pub use anthropic::AnthropicClient;
pub use client::LlmClient;
pub use streaming::{StreamEvent, StreamHandle};
pub use tool_parser::ToolParser;
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
