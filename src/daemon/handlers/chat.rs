//! Chat request handlers
//!
//! Handles chat.* IPC methods for interactive chat with the LLM.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::daemon::context::DaemonContext;
use crate::id::generate_loop_id;
use crate::ipc::messages::{DaemonError, DaemonEvent, DaemonResponse};
use crate::llm::{CompletionRequest, LlmClient, StopReason, ToolResult};
use crate::tools::ToolRouter;

/// System prompt for chat mode
const CHAT_SYSTEM_PROMPT: &str = r#"You are a helpful AI assistant integrated into Loopr, a loop-based task orchestration system.

You can help users with:
- General questions and conversations
- Code analysis and suggestions
- File operations (reading and writing files)
- Running bash commands

You have access to the following tools:
- bash: Execute bash commands
- read_file: Read file contents
- write_file: Write content to files

Be concise but helpful. When executing tools, explain what you're doing briefly."#;

/// Handle chat.send - send a message and get LLM response
pub async fn handle_chat_send(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let message = match params["message"].as_str() {
        Some(m) => m,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'message' parameter")),
    };

    // Check if LLM is ready
    if !ctx.llm_ready() {
        return DaemonResponse::error(
            id,
            DaemonError::internal_error("LLM client not ready - check ANTHROPIC_API_KEY"),
        );
    }

    // Add user message to session
    {
        let mut session = ctx.chat_session.write().await;
        session.add_user_message(message);
    }

    // Build completion request
    let request = {
        let session = ctx.chat_session.read().await;
        let mut req = CompletionRequest::new(CHAT_SYSTEM_PROMPT);
        for msg in &session.messages {
            req = req.with_message(msg.clone());
        }
        // Add tool definitions
        let tool_defs: Vec<_> = ctx
            .tool_router
            .available_tools()
            .into_iter()
            .filter_map(|name| {
                // Create basic tool definitions for the available tools
                match name.as_str() {
                    "bash" => Some(crate::llm::ToolDefinition::new(
                        "bash",
                        "Execute a bash command",
                        json!({
                            "type": "object",
                            "properties": {
                                "command": {
                                    "type": "string",
                                    "description": "The bash command to execute"
                                }
                            },
                            "required": ["command"]
                        }),
                    )),
                    "read_file" => Some(crate::llm::ToolDefinition::new(
                        "read_file",
                        "Read the contents of a file",
                        json!({
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Path to the file to read"
                                }
                            },
                            "required": ["path"]
                        }),
                    )),
                    "write_file" => Some(crate::llm::ToolDefinition::new(
                        "write_file",
                        "Write content to a file",
                        json!({
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Path to the file to write"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "Content to write to the file"
                                }
                            },
                            "required": ["path", "content"]
                        }),
                    )),
                    _ => None,
                }
            })
            .collect();
        req = req.with_tools(tool_defs);
        req
    };

    // Call LLM
    let response = match ctx.llm_client.complete(request.clone()).await {
        Ok(r) => r,
        Err(e) => {
            return DaemonResponse::error(id, DaemonError::internal_error(format!("LLM error: {}", e)));
        }
    };

    // Broadcast response text
    if !response.content.is_empty() {
        ctx.broadcast(DaemonEvent::chat_chunk(&response.content, false));
    }

    // Process tool calls if any
    let mut current_response = response;
    let mut current_request = request;
    let worktree = PathBuf::from(".");

    while current_response.stop_reason == StopReason::ToolUse && !current_response.tool_calls.is_empty() {
        let mut tool_results = Vec::new();

        for call in &current_response.tool_calls {
            // Broadcast tool call
            ctx.broadcast(DaemonEvent::chat_tool_call(&call.name, call.input.clone()));

            // Execute tool
            let result = ctx.tool_router.execute(call.clone(), &worktree).await;
            let tool_result = match result {
                Ok(r) => {
                    ctx.broadcast(DaemonEvent::chat_tool_result(&call.name, &r.content));
                    r
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    ctx.broadcast(DaemonEvent::chat_tool_result(&call.name, &error_msg));
                    ToolResult::error(&call.id, error_msg)
                }
            };
            tool_results.push(tool_result);
        }

        // Continue with tool results
        match ctx
            .llm_client
            .continue_with_tool_results(current_request.clone(), tool_results)
            .await
        {
            Ok(next_response) => {
                if !next_response.content.is_empty() {
                    ctx.broadcast(DaemonEvent::chat_chunk(&next_response.content, false));
                }
                current_response = next_response;
            }
            Err(e) => {
                return DaemonResponse::error(id, DaemonError::internal_error(format!("LLM error: {}", e)));
            }
        }

        // Update request for potential next iteration
        // Add assistant response and tool results to messages
        current_request = {
            let session = ctx.chat_session.read().await;
            let mut req = CompletionRequest::new(CHAT_SYSTEM_PROMPT);
            for msg in &session.messages {
                req = req.with_message(msg.clone());
            }
            req
        };
    }

    // Mark done
    ctx.broadcast(DaemonEvent::chat_chunk("", true));

    // Add assistant response to session
    {
        let mut session = ctx.chat_session.write().await;
        if !current_response.content.is_empty() {
            session.add_assistant_message(&current_response.content);
        }
        session.add_tokens(
            current_response.usage.input_tokens,
            current_response.usage.output_tokens,
        );
    }

    // Generate message ID and include response content
    let message_id = generate_loop_id();
    let response_content = current_response.content.clone();

    DaemonResponse::success(
        id,
        json!({
            "message_id": message_id,
            "response": response_content
        }),
    )
}

/// Handle chat.clear - clear chat history
pub async fn handle_chat_clear(id: u64, ctx: &DaemonContext) -> DaemonResponse {
    let mut session = ctx.chat_session.write().await;
    session.clear();
    DaemonResponse::success(id, json!({"cleared": true}))
}

/// Handle chat.cancel - cancel ongoing chat (placeholder for streaming)
pub async fn handle_chat_cancel(id: u64, _ctx: &DaemonContext) -> DaemonResponse {
    // For non-streaming implementation, this is a no-op
    // In a streaming implementation, this would cancel the ongoing request
    DaemonResponse::success(id, json!({"cancelled": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_system_prompt_not_empty() {
        assert!(!CHAT_SYSTEM_PROMPT.is_empty());
        assert!(CHAT_SYSTEM_PROMPT.contains("Loopr"));
    }
}
