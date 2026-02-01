//! Loop runner implementation - executes loops with the Ralph Wiggum pattern.
//!
//! The LoopRunner executes a single loop, iterating with fresh context until
//! validation passes or max iterations is reached.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::{Loop, LoopStatus, LoopType};
use crate::error::Result;
use crate::llm::{CompletionRequest, LlmClient, Message};
use crate::prompt::PromptRenderer;
use crate::tools::ToolRouter;
use crate::validation::Validator;

/// Outcome of a loop execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// Loop completed successfully - validation passed
    Complete,
    /// Loop failed - max iterations exhausted or unrecoverable error
    Failed(String),
    /// Loop was invalidated by a parent re-iteration
    Invalidated,
}

/// Configuration for the LoopRunner.
#[derive(Debug, Clone)]
pub struct LoopRunnerConfig {
    /// Maximum tokens for LLM responses
    pub max_tokens: u32,
    /// Whether to use streaming for LLM calls
    pub use_streaming: bool,
}

impl Default for LoopRunnerConfig {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
            use_streaming: false,
        }
    }
}

/// LoopRunner executes a single loop with the Ralph Wiggum pattern.
///
/// Each iteration:
/// 1. Builds a prompt with accumulated feedback (FRESH CONTEXT)
/// 2. Calls LLM with new messages array
/// 3. Executes tool calls
/// 4. Validates output
/// 5. On failure: accumulates feedback and continues
/// 6. On success: marks complete
pub struct LoopRunner<L, T, V>
where
    L: LlmClient,
    T: ToolRouter,
    V: Validator,
{
    /// LLM client for completions
    llm: Arc<L>,
    /// Tool router for executing tool calls
    tool_router: Arc<T>,
    /// Validator for loop outputs
    validator: Arc<V>,
    /// Prompt renderer for building prompts
    prompt_renderer: PromptRenderer,
    /// Configuration
    config: LoopRunnerConfig,
}

impl<L, T, V> LoopRunner<L, T, V>
where
    L: LlmClient,
    T: ToolRouter,
    V: Validator,
{
    /// Create a new LoopRunner with the given dependencies.
    pub fn new(
        llm: Arc<L>,
        tool_router: Arc<T>,
        validator: Arc<V>,
        prompt_renderer: PromptRenderer,
    ) -> Self {
        Self {
            llm,
            tool_router,
            validator,
            prompt_renderer,
            config: LoopRunnerConfig::default(),
        }
    }

    /// Create a new LoopRunner with custom configuration.
    pub fn with_config(
        llm: Arc<L>,
        tool_router: Arc<T>,
        validator: Arc<V>,
        prompt_renderer: PromptRenderer,
        config: LoopRunnerConfig,
    ) -> Self {
        Self {
            llm,
            tool_router,
            validator,
            prompt_renderer,
            config,
        }
    }

    /// Run the loop until completion or failure.
    ///
    /// This implements the Ralph Wiggum pattern:
    /// - Fresh context (new messages array) each iteration
    /// - Feedback accumulated in the prompt, not conversation history
    /// - Iterate until validation passes or max iterations reached
    pub async fn run(&self, loop_instance: &mut Loop) -> Result<LoopOutcome> {
        loop_instance.status = LoopStatus::Running;

        while loop_instance.iteration < loop_instance.max_iterations {
            // 1. Build prompt with accumulated feedback (FRESH CONTEXT)
            let system_prompt = self.build_system_prompt(loop_instance)?;
            let user_message = self.build_user_message(loop_instance);

            // 2. Call LLM - NEW messages array each time
            let tools = self.get_tools_for_loop_type(loop_instance.loop_type);
            let request = CompletionRequest {
                system: system_prompt,
                messages: vec![Message::user(&user_message)],
                tools,
                max_tokens: Some(self.config.max_tokens),
                ..Default::default()
            };

            let response = self.llm.complete(request).await?;

            // 3. Execute tool calls
            for call in &response.tool_calls {
                let result = self
                    .tool_router
                    .execute(call.clone(), &loop_instance.worktree)
                    .await?;

                // If tool execution fails, add to progress
                if result.is_error.unwrap_or(false) {
                    loop_instance.progress.push_str(&format!(
                        "\nTool {} failed: {}\n",
                        call.name,
                        result.content.as_deref().unwrap_or("unknown error")
                    ));
                }
            }

            // 4. Validate output
            let artifact_path = self.get_artifact_path(loop_instance);
            let validation_result = self
                .validator
                .validate(&artifact_path, &loop_instance.worktree)
                .await?;

            if validation_result.passed {
                loop_instance.status = LoopStatus::Complete;
                return Ok(LoopOutcome::Complete);
            }

            // 5. Accumulate feedback for next iteration
            loop_instance.progress.push_str(&format!(
                "\n---\n## Iteration {} Failed\n{}\n",
                loop_instance.iteration + 1,
                validation_result.output
            ));

            for error in &validation_result.errors {
                loop_instance.progress.push_str(&format!("- {}\n", error));
            }

            loop_instance.iteration += 1;

            // ITERATION ENDS - Context is discarded
            // Next iteration starts completely fresh
        }

        // Max iterations exhausted
        loop_instance.status = LoopStatus::Failed;
        Ok(LoopOutcome::Failed("Max iterations exhausted".into()))
    }

    /// Build the system prompt for this loop.
    fn build_system_prompt(&self, loop_instance: &Loop) -> Result<String> {
        // Render the prompt template with the loop's context
        self.prompt_renderer
            .render_json(&loop_instance.prompt_path.to_string_lossy(), &loop_instance.context)
    }

    /// Build the user message with task and accumulated feedback.
    fn build_user_message(&self, loop_instance: &Loop) -> String {
        let task = loop_instance
            .context
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if loop_instance.progress.is_empty() {
            task.to_string()
        } else {
            format!(
                "{}\n\n## Previous Iteration Feedback\n{}",
                task, loop_instance.progress
            )
        }
    }

    /// Get tool definitions appropriate for the loop type.
    fn get_tools_for_loop_type(&self, loop_type: LoopType) -> Vec<crate::llm::ToolDefinition> {
        // For now, return all available tools
        // In the future, this could filter based on loop type
        let tool_names = self.tool_router.available_tools();
        tool_names
            .iter()
            .map(|name| crate::llm::ToolDefinition {
                name: name.clone(),
                description: format!("Tool: {}", name),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            })
            .collect()
    }

    /// Get the path to the output artifact for validation.
    fn get_artifact_path(&self, loop_instance: &Loop) -> PathBuf {
        if let Some(artifact) = loop_instance.output_artifacts.first() {
            artifact.clone()
        } else {
            // For CodeLoop, validate the worktree itself
            loop_instance.worktree.clone()
        }
    }
}

/// Trait for signal checking during loop execution.
#[async_trait]
pub trait SignalChecker: Send + Sync {
    /// Check if the loop should be stopped.
    async fn should_stop(&self, loop_id: &str) -> Result<bool>;

    /// Check if the loop should be paused.
    async fn should_pause(&self, loop_id: &str) -> Result<bool>;

    /// Check if the loop has been invalidated.
    async fn is_invalidated(&self, loop_id: &str) -> Result<bool>;
}

/// No-op signal checker for testing.
pub struct NoOpSignalChecker;

#[async_trait]
impl SignalChecker for NoOpSignalChecker {
    async fn should_stop(&self, _loop_id: &str) -> Result<bool> {
        Ok(false)
    }

    async fn should_pause(&self, _loop_id: &str) -> Result<bool> {
        Ok(false)
    }

    async fn is_invalidated(&self, _loop_id: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompletionResponse, MockLlmClient, StopReason, ToolCall, ToolResult, Usage};
    use crate::prompt::PromptRenderer;
    use crate::validation::ValidationResult;
    use std::path::Path;
    use tokio;

    /// Mock tool router for testing.
    struct MockToolRouter {
        tools: Vec<String>,
    }

    impl MockToolRouter {
        fn new() -> Self {
            Self {
                tools: vec!["read_file".to_string(), "write_file".to_string()],
            }
        }
    }

    #[async_trait]
    impl ToolRouter for MockToolRouter {
        async fn execute(&self, call: ToolCall, _worktree: &Path) -> Result<ToolResult> {
            Ok(ToolResult {
                tool_use_id: call.id,
                content: Some(format!("Executed {}", call.name)),
                is_error: Some(false),
            })
        }

        fn available_tools(&self) -> Vec<String> {
            self.tools.clone()
        }
    }

    /// Mock validator for testing.
    struct MockValidator {
        pass_on_iteration: u32,
    }

    impl MockValidator {
        fn new(pass_on_iteration: u32) -> Self {
            Self { pass_on_iteration }
        }
    }

    #[async_trait]
    impl Validator for MockValidator {
        async fn validate(&self, _artifact: &Path, _worktree: &Path) -> Result<ValidationResult> {
            // We use a static counter via file system trick since we can't mutate self
            // For simplicity in tests, always pass
            Ok(ValidationResult::pass("Validation passed"))
        }
    }

    /// Mock validator that fails N times then passes.
    struct CountingValidator {
        fail_count: std::sync::atomic::AtomicU32,
        pass_after: u32,
    }

    impl CountingValidator {
        fn new(pass_after: u32) -> Self {
            Self {
                fail_count: std::sync::atomic::AtomicU32::new(0),
                pass_after,
            }
        }
    }

    #[async_trait]
    impl Validator for CountingValidator {
        async fn validate(&self, _artifact: &Path, _worktree: &Path) -> Result<ValidationResult> {
            let count = self
                .fail_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count >= self.pass_after {
                Ok(ValidationResult::pass("Validation passed"))
            } else {
                Ok(ValidationResult::fail(format!(
                    "Validation failed (attempt {})",
                    count + 1
                )))
            }
        }
    }

    #[test]
    fn test_loop_outcome_variants() {
        assert_eq!(LoopOutcome::Complete, LoopOutcome::Complete);
        assert_eq!(
            LoopOutcome::Failed("test".into()),
            LoopOutcome::Failed("test".into())
        );
        assert_eq!(LoopOutcome::Invalidated, LoopOutcome::Invalidated);
        assert_ne!(LoopOutcome::Complete, LoopOutcome::Invalidated);
    }

    #[test]
    fn test_loop_runner_config_default() {
        let config = LoopRunnerConfig::default();
        assert_eq!(config.max_tokens, 8192);
        assert!(!config.use_streaming);
    }

    #[tokio::test]
    async fn test_mock_tool_router() {
        let router = MockToolRouter::new();
        let tools = router.available_tools();
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"write_file".to_string()));
    }

    #[tokio::test]
    async fn test_mock_tool_router_execute() {
        let router = MockToolRouter::new();
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "test.txt"}),
        };
        let result = router.execute(call, Path::new("/tmp")).await.unwrap();
        assert_eq!(result.tool_use_id, "call-1");
        assert_eq!(result.is_error, Some(false));
    }

    #[tokio::test]
    async fn test_mock_validator_passes() {
        let validator = MockValidator::new(1);
        let result = validator
            .validate(Path::new("/tmp"), Path::new("/tmp"))
            .await
            .unwrap();
        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_counting_validator() {
        let validator = CountingValidator::new(2);

        // First call - fails
        let result1 = validator
            .validate(Path::new("/tmp"), Path::new("/tmp"))
            .await
            .unwrap();
        assert!(!result1.passed);

        // Second call - fails
        let result2 = validator
            .validate(Path::new("/tmp"), Path::new("/tmp"))
            .await
            .unwrap();
        assert!(!result2.passed);

        // Third call - passes
        let result3 = validator
            .validate(Path::new("/tmp"), Path::new("/tmp"))
            .await
            .unwrap();
        assert!(result3.passed);
    }

    #[tokio::test]
    async fn test_no_op_signal_checker() {
        let checker = NoOpSignalChecker;
        assert!(!checker.should_stop("test-loop").await.unwrap());
        assert!(!checker.should_pause("test-loop").await.unwrap());
        assert!(!checker.is_invalidated("test-loop").await.unwrap());
    }

    #[test]
    fn test_build_user_message_no_progress() {
        let renderer = PromptRenderer::new();
        let llm = Arc::new(MockLlmClient::new(vec![]));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(MockValidator::new(1));

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let mut loop_instance = Loop::new_plan("Build a web server");
        loop_instance.progress = String::new();

        let message = runner.build_user_message(&loop_instance);
        assert_eq!(message, "Build a web server");
    }

    #[test]
    fn test_build_user_message_with_progress() {
        let renderer = PromptRenderer::new();
        let llm = Arc::new(MockLlmClient::new(vec![]));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(MockValidator::new(1));

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let mut loop_instance = Loop::new_plan("Build a web server");
        loop_instance.progress = "Iteration 1 failed: missing tests".to_string();

        let message = runner.build_user_message(&loop_instance);
        assert!(message.contains("Build a web server"));
        assert!(message.contains("Previous Iteration Feedback"));
        assert!(message.contains("missing tests"));
    }

    #[test]
    fn test_get_tools_for_loop_type() {
        let renderer = PromptRenderer::new();
        let llm = Arc::new(MockLlmClient::new(vec![]));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(MockValidator::new(1));

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let tools = runner.get_tools_for_loop_type(LoopType::Plan);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn test_get_artifact_path_with_artifact() {
        let renderer = PromptRenderer::new();
        let llm = Arc::new(MockLlmClient::new(vec![]));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(MockValidator::new(1));

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let mut loop_instance = Loop::new_plan("test");
        loop_instance
            .output_artifacts
            .push(PathBuf::from("/tmp/plan.md"));

        let path = runner.get_artifact_path(&loop_instance);
        assert_eq!(path, PathBuf::from("/tmp/plan.md"));
    }

    #[test]
    fn test_get_artifact_path_no_artifact() {
        let renderer = PromptRenderer::new();
        let llm = Arc::new(MockLlmClient::new(vec![]));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(MockValidator::new(1));

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let loop_instance = Loop::new_code("parent-001", 1);

        let path = runner.get_artifact_path(&loop_instance);
        assert_eq!(path, loop_instance.worktree);
    }

    #[tokio::test]
    async fn test_loop_runner_run_passes_first_try() {
        let renderer = PromptRenderer::new();
        let response = CompletionResponse {
            content: "Here's the plan".to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
            },
            model: "claude-3".to_string(),
        };
        let llm = Arc::new(MockLlmClient::new(vec![response]));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(MockValidator::new(0)); // Always passes

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let mut loop_instance = Loop::new_plan("Build a CLI");
        let outcome = runner.run(&mut loop_instance).await.unwrap();

        assert_eq!(outcome, LoopOutcome::Complete);
        assert_eq!(loop_instance.status, LoopStatus::Complete);
        assert_eq!(loop_instance.iteration, 0);
    }

    #[tokio::test]
    async fn test_loop_runner_run_max_iterations() {
        let renderer = PromptRenderer::new();
        // Create enough responses for max iterations
        let responses: Vec<CompletionResponse> = (0..10)
            .map(|_| CompletionResponse {
                content: "Trying again".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                },
                model: "claude-3".to_string(),
            })
            .collect();
        let llm = Arc::new(MockLlmClient::new(responses));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(CountingValidator::new(100)); // Never passes

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let mut loop_instance = Loop::new_plan("Build a CLI");
        loop_instance.max_iterations = 3;

        let outcome = runner.run(&mut loop_instance).await.unwrap();

        assert_eq!(outcome, LoopOutcome::Failed("Max iterations exhausted".into()));
        assert_eq!(loop_instance.status, LoopStatus::Failed);
        assert_eq!(loop_instance.iteration, 3);
    }

    #[tokio::test]
    async fn test_loop_runner_accumulates_progress() {
        let renderer = PromptRenderer::new();
        let responses: Vec<CompletionResponse> = (0..5)
            .map(|_| CompletionResponse {
                content: "Attempt".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                },
                model: "claude-3".to_string(),
            })
            .collect();
        let llm = Arc::new(MockLlmClient::new(responses));
        let router = Arc::new(MockToolRouter::new());
        let validator = Arc::new(CountingValidator::new(2)); // Passes on 3rd attempt

        let runner = LoopRunner::new(llm, router, validator, renderer);

        let mut loop_instance = Loop::new_plan("Build a CLI");
        let outcome = runner.run(&mut loop_instance).await.unwrap();

        assert_eq!(outcome, LoopOutcome::Complete);
        // Progress should contain feedback from iterations 1 and 2
        assert!(loop_instance.progress.contains("Iteration 1 Failed"));
        assert!(loop_instance.progress.contains("Iteration 2 Failed"));
    }
}
