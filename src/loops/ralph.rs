//! RalphLoop - The leaf-level loop that does actual coding work.
//!
//! Named after the "Ralph Wiggum" technique: iterate until validation passes.
//! Each iteration: prompt → LLM → tools → validate → (repeat if failed)

use std::path::PathBuf;
use thiserror::Error;

use crate::llm::{
    CompletionRequest, ContentBlock, LlmClient, LlmError, Message, MessageContent, Role, StopReason, ToolContext,
    ToolDefinition, ToolExecutor,
};
use crate::store::{LoopRecord, LoopStatus};

use super::validation::{ValidationConfig, ValidationFeedback, ValidationResult, Validator};
use super::worktree::{Worktree, WorktreeConfig, WorktreeError};

/// Errors that can occur during loop execution
#[derive(Debug, Error)]
pub enum LoopError {
    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("Worktree error: {0}")]
    Worktree(#[from] WorktreeError),

    #[error("Store error: {0}")]
    Store(String),

    #[error("Max iterations ({0}) reached")]
    MaxIterations(u32),

    #[error("Loop was stopped")]
    Stopped,

    #[error("Loop was invalidated by parent")]
    Invalidated,

    #[error("Missing task in context")]
    MissingTask,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for RalphLoop execution
#[derive(Debug, Clone)]
pub struct RalphLoopConfig {
    /// Worktree configuration
    pub worktree: WorktreeConfig,

    /// Validation configuration
    pub validation: ValidationConfig,

    /// System prompt template for the loop
    pub system_prompt: String,

    /// Maximum tokens for LLM response
    pub max_tokens: u32,
}

impl Default for RalphLoopConfig {
    fn default() -> Self {
        Self {
            worktree: WorktreeConfig::default(),
            validation: ValidationConfig::default(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_tokens: 16384,
        }
    }
}

/// Default system prompt for Ralph loops
const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a coding assistant executing a focused task. Your goal is to complete the task and ensure validation passes.

## Available Tools
You have access to file operations (read, write, edit), shell commands, and directory listing.

## Workflow
1. Read relevant files to understand the codebase
2. Make necessary changes to complete the task
3. Ensure your changes will pass validation

## Important
- Make minimal, focused changes
- Don't modify files unnecessarily
- Follow existing code patterns
- Ensure tests pass"#;

/// Action to take after an iteration
#[derive(Debug)]
pub enum LoopAction {
    /// Continue to next iteration
    Continue,

    /// Loop completed successfully
    Complete,

    /// Loop failed with the given reason
    Fail(String),
}

/// The Ralph loop implementation
pub struct RalphLoop {
    /// The loop record from TaskStore
    pub record: LoopRecord,

    /// The worktree for this loop
    worktree: Option<Worktree>,

    /// Configuration
    config: RalphLoopConfig,

    /// Accumulated feedback from failed iterations
    iteration_history: Vec<IterationResult>,
}

/// Result of a single iteration
#[derive(Debug, Clone)]
pub struct IterationResult {
    /// Iteration number (1-indexed)
    pub iteration: u32,

    /// Whether validation passed
    pub passed: bool,

    /// Validation feedback (if failed)
    pub feedback: Option<ValidationFeedback>,

    /// Token usage for this iteration
    pub tokens_used: u64,
}

impl RalphLoop {
    /// Create a new RalphLoop from a LoopRecord
    pub fn new(record: LoopRecord, config: RalphLoopConfig) -> Self {
        Self {
            record,
            worktree: None,
            config,
            iteration_history: Vec::new(),
        }
    }

    /// Get the current iteration number
    pub fn iteration(&self) -> u32 {
        self.record.iteration
    }

    /// Get the task description from the context
    pub fn task(&self) -> Option<&str> {
        self.record.context.get("task").and_then(|v| v.as_str())
    }

    /// Get the worktree path (if initialized)
    pub fn worktree_path(&self) -> Option<&PathBuf> {
        self.worktree.as_ref().map(|w| &w.path)
    }

    /// Initialize the worktree for this loop
    pub async fn init_worktree(&mut self) -> Result<(), LoopError> {
        if self.worktree.is_some() {
            return Ok(());
        }

        let worktree = Worktree::create(&self.record.id, self.config.worktree.clone()).await?;
        self.worktree = Some(worktree);
        Ok(())
    }

    /// Build the user prompt for the current iteration
    fn build_user_prompt(&self) -> Result<String, LoopError> {
        let task = self.task().ok_or(LoopError::MissingTask)?;

        let mut prompt = format!("## Task\n\n{}\n\n", task);

        // Add iteration history if this isn't the first iteration
        if !self.iteration_history.is_empty() {
            prompt.push_str("## Previous Iterations\n\n");

            for result in &self.iteration_history {
                prompt.push_str(&format!("### Iteration {}\n", result.iteration));

                if let Some(feedback) = &result.feedback {
                    prompt.push_str(&feedback.format_for_prompt());
                }

                prompt.push('\n');
            }

            prompt.push_str("**Fix the issues from the previous iteration(s) and ensure validation passes.**\n\n");
        }

        Ok(prompt)
    }

    /// Build the completion request for an iteration
    fn build_request(&self, tools: Vec<ToolDefinition>) -> Result<CompletionRequest, LoopError> {
        let user_prompt = self.build_user_prompt()?;

        Ok(CompletionRequest {
            system_prompt: self.config.system_prompt.clone(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_prompt),
            }],
            tools,
            max_tokens: self.config.max_tokens,
        })
    }

    /// Handle the validation result and determine next action
    pub fn handle_validation(&mut self, result: ValidationResult) -> LoopAction {
        let iteration_result = IterationResult {
            iteration: self.record.iteration,
            passed: result.passed(),
            feedback: result.feedback().cloned(),
            tokens_used: 0, // Will be updated during execution
        };

        self.iteration_history.push(iteration_result);

        if result.passed() {
            LoopAction::Complete
        } else if self.record.iteration >= self.record.max_iterations {
            LoopAction::Fail(format!(
                "Max iterations ({}) reached without passing validation",
                self.record.max_iterations
            ))
        } else {
            LoopAction::Continue
        }
    }

    /// Cleanup the worktree
    pub async fn cleanup(self) -> Result<(), LoopError> {
        if let Some(worktree) = self.worktree {
            worktree.cleanup().await?;
        }
        Ok(())
    }
}

/// Loop runner that handles iteration execution
///
/// Note: TaskStore is not Sync (uses RefCell internally), so full loop orchestration
/// will be handled by a single-threaded executor in Phase 6. For Phase 3, we provide
/// the core iteration logic.
pub struct LoopRunner {
    validator: Validator,
}

impl LoopRunner {
    /// Create a new loop runner with the given validation config
    pub fn new(validation_config: ValidationConfig) -> Self {
        Self {
            validator: Validator::new(validation_config),
        }
    }

    /// Create a runner with default validation config
    pub fn default_runner() -> Self {
        Self::new(ValidationConfig::default())
    }

    /// Run one iteration of the loop
    ///
    /// This is the core iteration logic:
    /// 1. Build prompt with task and iteration history
    /// 2. Send to LLM
    /// 3. Execute tool calls until LLM is done
    /// 4. Run validation
    /// 5. Return result
    pub async fn run_iteration(
        &self,
        loop_state: &mut RalphLoop,
        llm: &dyn LlmClient,
        executor: &ToolExecutor,
    ) -> Result<ValidationResult, LoopError> {
        // Increment iteration
        loop_state.record.iteration += 1;
        loop_state.record.touch();

        // Ensure worktree is initialized
        loop_state.init_worktree().await?;
        let worktree_path = loop_state.worktree_path().unwrap().clone();

        // Create tool context for this worktree
        let tool_context = ToolContext::new(worktree_path.clone(), loop_state.record.id.clone());

        // Get tool definitions
        let tool_definitions = executor.definitions();

        // Build and send the request
        let request = loop_state.build_request(tool_definitions)?;
        let mut messages = request.messages.clone();

        // LLM conversation loop (handle tool calls)
        loop {
            let response = llm
                .complete(CompletionRequest {
                    system_prompt: request.system_prompt.clone(),
                    messages: messages.clone(),
                    tools: request.tools.clone(),
                    max_tokens: request.max_tokens,
                })
                .await?;

            // Process the response
            if response.tool_calls.is_empty() {
                // No more tool calls, LLM is done
                break;
            }

            // Build assistant message with tool calls
            let mut content_blocks: Vec<ContentBlock> = Vec::new();

            if let Some(text) = &response.content
                && !text.is_empty()
            {
                content_blocks.push(ContentBlock::Text { text: text.clone() });
            }

            for tool_call in &response.tool_calls {
                content_blocks.push(ContentBlock::ToolUse {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    input: tool_call.input.clone(),
                });
            }

            messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(content_blocks),
            });

            // Execute tools and build result message
            let mut result_blocks: Vec<ContentBlock> = Vec::new();

            for tool_call in &response.tool_calls {
                let result = executor.execute(tool_call, &tool_context).await;

                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: tool_call.id.clone(),
                    content: result.content,
                    is_error: result.is_error,
                });
            }

            messages.push(Message {
                role: Role::User,
                content: MessageContent::Blocks(result_blocks),
            });

            // Check stop reason
            if response.stop_reason == StopReason::EndTurn {
                break;
            }
        }

        // Run validation
        let validation_result = self.validator.validate(&worktree_path).await.map_err(|e| {
            // Convert validation error to LoopError
            LoopError::Store(format!("Validation error: {}", e))
        })?;

        Ok(validation_result)
    }

    /// Get the validator for external use
    pub fn validator(&self) -> &Validator {
        &self.validator
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LoopType;

    #[test]
    fn test_ralph_loop_new() {
        let record = LoopRecord::new_ralph("Test task", 5);
        let config = RalphLoopConfig::default();
        let loop_state = RalphLoop::new(record, config);

        assert_eq!(loop_state.iteration(), 0);
        assert_eq!(loop_state.task(), Some("Test task"));
        assert!(loop_state.worktree_path().is_none());
    }

    #[test]
    fn test_ralph_loop_task_extraction() {
        let record = LoopRecord::new_ralph("Build a REST API", 10);
        let config = RalphLoopConfig::default();
        let loop_state = RalphLoop::new(record, config);

        assert_eq!(loop_state.task(), Some("Build a REST API"));
    }

    #[test]
    fn test_build_user_prompt_first_iteration() {
        let record = LoopRecord::new_ralph("Implement the feature", 5);
        let config = RalphLoopConfig::default();
        let loop_state = RalphLoop::new(record, config);

        let prompt = loop_state.build_user_prompt().unwrap();

        assert!(prompt.contains("Implement the feature"));
        assert!(!prompt.contains("Previous Iterations"));
    }

    #[test]
    fn test_build_user_prompt_with_history() {
        let record = LoopRecord::new_ralph("Fix the bug", 5);
        let config = RalphLoopConfig::default();
        let mut loop_state = RalphLoop::new(record, config);

        // Add iteration history
        loop_state.iteration_history.push(IterationResult {
            iteration: 1,
            passed: false,
            feedback: Some(ValidationFeedback::from_command_output(
                Some(1),
                "".into(),
                "error: test failed".into(),
            )),
            tokens_used: 1000,
        });

        let prompt = loop_state.build_user_prompt().unwrap();

        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("Previous Iterations"));
        assert!(prompt.contains("Iteration 1"));
        assert!(prompt.contains("test failed"));
    }

    #[test]
    fn test_handle_validation_pass() {
        let record = LoopRecord::new_ralph("Task", 5);
        let config = RalphLoopConfig::default();
        let mut loop_state = RalphLoop::new(record, config);
        loop_state.record.iteration = 1;

        let action = loop_state.handle_validation(ValidationResult::Pass);

        assert!(matches!(action, LoopAction::Complete));
        assert_eq!(loop_state.iteration_history.len(), 1);
        assert!(loop_state.iteration_history[0].passed);
    }

    #[test]
    fn test_handle_validation_fail_continue() {
        let record = LoopRecord::new_ralph("Task", 5);
        let config = RalphLoopConfig::default();
        let mut loop_state = RalphLoop::new(record, config);
        loop_state.record.iteration = 1;

        let action = loop_state.handle_validation(ValidationResult::Fail(ValidationFeedback::timeout()));

        assert!(matches!(action, LoopAction::Continue));
        assert!(!loop_state.iteration_history[0].passed);
    }

    #[test]
    fn test_handle_validation_max_iterations() {
        let record = LoopRecord::new_ralph("Task", 3);
        let config = RalphLoopConfig::default();
        let mut loop_state = RalphLoop::new(record, config);
        loop_state.record.iteration = 3; // At max

        let action = loop_state.handle_validation(ValidationResult::Fail(ValidationFeedback::timeout()));

        assert!(matches!(action, LoopAction::Fail(_)));
    }

    #[test]
    fn test_ralph_loop_config_default() {
        let config = RalphLoopConfig::default();
        assert_eq!(config.max_tokens, 16384);
        assert!(config.system_prompt.contains("coding assistant"));
    }

    #[test]
    fn test_loop_action_variants() {
        let continue_action = LoopAction::Continue;
        let complete_action = LoopAction::Complete;
        let fail_action = LoopAction::Fail("test".to_string());

        assert!(matches!(continue_action, LoopAction::Continue));
        assert!(matches!(complete_action, LoopAction::Complete));
        assert!(matches!(fail_action, LoopAction::Fail(_)));
    }

    #[test]
    fn test_iteration_result() {
        let result = IterationResult {
            iteration: 1,
            passed: true,
            feedback: None,
            tokens_used: 500,
        };

        assert_eq!(result.iteration, 1);
        assert!(result.passed);
        assert!(result.feedback.is_none());
    }

    #[test]
    fn test_missing_task_error() {
        let mut record = LoopRecord::new(LoopType::Ralph, serde_json::json!({}));
        record.context = serde_json::json!({}); // Empty context
        let config = RalphLoopConfig::default();
        let loop_state = RalphLoop::new(record, config);

        let result = loop_state.build_user_prompt();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoopError::MissingTask));
    }
}
