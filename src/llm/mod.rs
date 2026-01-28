//! LLM Client module for Loopr
//!
//! Provides the LlmClient trait and implementations for interacting with LLM APIs.
//! Currently supports Anthropic's Claude API with streaming support.
//!
//! Note: dead_code/unused warnings are expected during Phase 2 development.
//! These will be cleaned up when the module is integrated in later phases.

#![allow(dead_code)]
#![allow(unused_imports)]

pub mod anthropic;
pub mod client;
pub mod tools;

pub use anthropic::AnthropicClient;
pub use client::{
    CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmError, Message, MessageContent, Role,
    StopReason, StreamChunk, TokenUsage, ToolCall,
};
pub use tools::{Tool, ToolContext, ToolDefinition, ToolError, ToolExecutor, ToolResult};
