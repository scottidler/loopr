//! Rule of Five Executor: Coordinates the 5-pass review process.
//!
//! The executor manages the state of a plan through all five review passes,
//! tracking progress and feedback between passes.

use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, Message, MessageContent, Role, ToolDefinition};
use crate::store::{LoopRecord, LoopStatus};

use super::TOTAL_PASSES;
use super::passes::{ReviewPass, get_pass_prompt};
use super::validation::{PassValidationResult, validate_pass};

/// Configuration for the Rule of Five executor.
#[derive(Debug, Clone)]
pub struct RuleOfFiveConfig {
    /// Maximum tokens for LLM response per pass.
    pub max_tokens: u32,
    /// Maximum iterations per pass before giving up.
    pub max_iterations_per_pass: u32,
}

impl Default for RuleOfFiveConfig {
    fn default() -> Self {
        Self {
            max_tokens: 16384,
            max_iterations_per_pass: 3,
        }
    }
}

/// Result of executing a single pass.
#[derive(Debug, Clone)]
pub struct PassResult {
    /// Which pass was executed.
    pub pass: ReviewPass,
    /// Whether the pass succeeded.
    pub passed: bool,
    /// The LLM's response.
    pub response: String,
    /// The updated plan content (if the plan was revised).
    pub updated_plan: Option<String>,
    /// Issues found (if any).
    pub issues: Vec<String>,
    /// Number of iterations it took.
    pub iterations: u32,
}

/// Executes the Rule of Five review process on a Plan.
pub struct RuleOfFiveExecutor {
    /// Configuration.
    config: RuleOfFiveConfig,
    /// Current pass (1-5).
    current_pass: u32,
    /// Current plan content.
    plan_content: String,
    /// History of pass results.
    pass_history: Vec<PassResult>,
    /// Total iterations across all passes.
    total_iterations: u32,
}

impl RuleOfFiveExecutor {
    /// Create a new executor for a plan.
    pub fn new(initial_plan: String, config: RuleOfFiveConfig) -> Self {
        Self {
            config,
            current_pass: 1,
            plan_content: initial_plan,
            pass_history: Vec::with_capacity(5),
            total_iterations: 0,
        }
    }

    /// Create from a LoopRecord (resumes from saved state).
    pub fn from_record(record: &LoopRecord, config: RuleOfFiveConfig) -> Option<Self> {
        // Get the current plan content from context
        let plan_content = record
            .context
            .get("plan_content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // Get the current pass from context
        let current_pass = record.context.get("review_pass").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        Some(Self {
            config,
            current_pass,
            plan_content,
            pass_history: Vec::new(),
            total_iterations: record.iteration,
        })
    }

    /// Get the current pass.
    pub fn current_pass(&self) -> Option<ReviewPass> {
        ReviewPass::from_number(self.current_pass)
    }

    /// Get the current plan content.
    pub fn plan_content(&self) -> &str {
        &self.plan_content
    }

    /// Get the pass history.
    pub fn pass_history(&self) -> &[PassResult] {
        &self.pass_history
    }

    /// Check if all passes are complete.
    pub fn is_complete(&self) -> bool {
        self.current_pass > TOTAL_PASSES
    }

    /// Get the total number of iterations across all passes.
    pub fn total_iterations(&self) -> u32 {
        self.total_iterations
    }

    /// Build a completion request for the current pass.
    pub fn build_request(&self, tools: Vec<ToolDefinition>) -> Option<CompletionRequest> {
        let pass = self.current_pass()?;
        let prompt = get_pass_prompt(pass);

        let user_prompt = prompt.render_user_prompt(&self.plan_content);

        Some(CompletionRequest {
            system_prompt: prompt.system.clone(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_prompt),
            }],
            tools,
            max_tokens: self.config.max_tokens,
        })
    }

    /// Process the LLM response for the current pass.
    ///
    /// Returns the result of this pass and whether we should continue.
    pub fn process_response(&mut self, response: &str, updated_plan: Option<String>) -> PassResult {
        let pass = self.current_pass().expect("process_response called after completion");
        self.total_iterations += 1;

        // Use updated plan if provided
        let plan_for_validation = updated_plan.as_ref().unwrap_or(&self.plan_content);

        // Validate the response
        let validation = validate_pass(pass, response, plan_for_validation);

        let (passed, issues) = match validation {
            PassValidationResult::Passed => (true, Vec::new()),
            PassValidationResult::NeedsWork(issues) => (false, issues),
            PassValidationResult::Error(e) => (false, vec![e]),
        };

        let result = PassResult {
            pass,
            passed,
            response: response.to_string(),
            updated_plan: updated_plan.clone(),
            issues: issues.clone(),
            iterations: 1, // Will be accumulated in execute_pass
        };

        if passed {
            // Update plan content if revised
            if let Some(new_content) = updated_plan {
                self.plan_content = new_content;
            }

            // Advance to next pass
            self.current_pass += 1;
        }

        result
    }

    /// Execute a single pass, potentially with multiple iterations.
    ///
    /// Returns the final result for this pass.
    pub async fn execute_pass<C: LlmClient>(
        &mut self,
        client: &C,
        tools: Vec<ToolDefinition>,
    ) -> Result<PassResult, ExecutorError> {
        let pass = self.current_pass().ok_or(ExecutorError::AlreadyComplete)?;

        let mut iterations = 0;
        let mut last_issues = Vec::new();

        while iterations < self.config.max_iterations_per_pass {
            iterations += 1;

            let request = self
                .build_request(tools.clone())
                .ok_or(ExecutorError::AlreadyComplete)?;

            // Call the LLM
            let response = client
                .complete(request)
                .await
                .map_err(|e| ExecutorError::LlmError(e.to_string()))?;

            // Extract text from response
            let response_text = extract_text_from_response(&response);

            // Process the response
            let mut result = self.process_response(&response_text, None);
            result.iterations = iterations;

            if result.passed {
                self.pass_history.push(result.clone());
                return Ok(result);
            }

            last_issues = result.issues.clone();

            // If not passed, we might try again (the prompt will include issues)
            // For now, we just return after max iterations
        }

        // Max iterations reached without passing
        let result = PassResult {
            pass,
            passed: false,
            response: format!("Max iterations ({}) reached", self.config.max_iterations_per_pass),
            updated_plan: None,
            issues: last_issues,
            iterations,
        };

        self.pass_history.push(result.clone());
        Ok(result)
    }

    /// Execute all remaining passes.
    pub async fn execute_all<C: LlmClient>(
        &mut self,
        client: &C,
        tools: Vec<ToolDefinition>,
    ) -> Result<Vec<PassResult>, ExecutorError> {
        let mut results = Vec::new();

        while !self.is_complete() {
            let result = self.execute_pass(client, tools.clone()).await?;
            let passed = result.passed;
            results.push(result);

            if !passed {
                // Stop if a pass fails
                break;
            }
        }

        Ok(results)
    }

    /// Update the loop record with current state.
    pub fn update_record(&self, record: &mut LoopRecord) {
        record.iteration = self.total_iterations;

        // Update context with current pass and plan
        if let Some(ctx) = record.context.as_object_mut() {
            ctx.insert("review_pass".to_string(), serde_json::json!(self.current_pass));
            ctx.insert("plan_content".to_string(), serde_json::json!(self.plan_content));

            // Store pass history summary
            let history: Vec<_> = self
                .pass_history
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "pass": r.pass.number(),
                        "passed": r.passed,
                        "iterations": r.iterations,
                    })
                })
                .collect();
            ctx.insert("pass_history".to_string(), serde_json::json!(history));
        }

        // Update status based on completion
        if self.is_complete() {
            record.status = LoopStatus::Complete;
        }
    }
}

/// Errors that can occur during execution.
#[derive(Debug)]
pub enum ExecutorError {
    /// All passes already complete.
    AlreadyComplete,
    /// LLM call failed.
    LlmError(String),
    /// Invalid state.
    InvalidState(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyComplete => write!(f, "All passes already complete"),
            Self::LlmError(e) => write!(f, "LLM error: {}", e),
            Self::InvalidState(s) => write!(f, "Invalid state: {}", s),
        }
    }
}

impl std::error::Error for ExecutorError {}

/// Extract text content from LLM response.
fn extract_text_from_response(response: &CompletionResponse) -> String {
    response.content.clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{StopReason, TokenUsage};

    fn make_test_plan() -> String {
        r#"# Plan: Test Plan

## Summary
A test plan for building a feature with complete documentation.

## Goals
- Goal 1: Build the feature
- Goal 2: Test thoroughly

## Non-Goals
- Not doing extra work

## Proposed Solution

### Overview
We will implement the feature using best practices and proper architecture.

### Key Components
- Component A
- Component B

## Specs

### Spec 1: Core
Core functionality.

**Scope:**
- Item 1
- Item 2

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Failure | Low | High | Handle it |
"#
        .to_string()
    }

    #[test]
    fn test_executor_new() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let executor = RuleOfFiveExecutor::new(plan.clone(), config);

        assert_eq!(executor.current_pass(), Some(ReviewPass::Completeness));
        assert_eq!(executor.plan_content(), plan);
        assert!(!executor.is_complete());
    }

    #[test]
    fn test_executor_build_request() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let executor = RuleOfFiveExecutor::new(plan, config);

        let request = executor.build_request(vec![]).unwrap();

        // Should have system prompt for pass 1
        assert!(request.system_prompt.contains("COMPLETENESS"));
        // User message should contain plan
        match &request.messages[0].content {
            MessageContent::Text(text) => {
                assert!(text.contains("## Summary"));
            }
            _ => panic!("Expected text message"),
        }
    }

    #[test]
    fn test_executor_process_response_pass() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let mut executor = RuleOfFiveExecutor::new(plan, config);

        let response = "PASS: All required sections present and filled.";
        let result = executor.process_response(response, None);

        assert!(result.passed);
        assert!(result.issues.is_empty());
        assert_eq!(executor.current_pass(), Some(ReviewPass::Correctness));
    }

    #[test]
    fn test_executor_process_response_needs_work() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let mut executor = RuleOfFiveExecutor::new(plan, config);

        let response = "NEEDS_WORK:\n1. Missing detailed acceptance criteria\n2. Risks section incomplete";
        let result = executor.process_response(response, None);

        assert!(!result.passed);
        assert_eq!(result.issues.len(), 2);
        assert_eq!(executor.current_pass(), Some(ReviewPass::Completeness)); // Still on pass 1
    }

    #[test]
    fn test_executor_is_complete() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let mut executor = RuleOfFiveExecutor::new(plan, config);

        // Advance through all passes
        for _ in 0..5 {
            assert!(!executor.is_complete());
            executor.process_response("PASS: Good.", None);
        }

        assert!(executor.is_complete());
        assert_eq!(executor.current_pass(), None);
    }

    #[test]
    fn test_executor_update_record() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let mut executor = RuleOfFiveExecutor::new(plan.clone(), config);

        // Advance to pass 2
        executor.process_response("PASS: Good.", None);

        let mut record = LoopRecord::new_plan("test task", 10);
        executor.update_record(&mut record);

        assert_eq!(record.iteration, 1);
        assert_eq!(record.context.get("review_pass").and_then(|v| v.as_u64()), Some(2));
    }

    #[test]
    fn test_executor_from_record() {
        let mut record = LoopRecord::new_plan("test task", 10);
        if let Some(ctx) = record.context.as_object_mut() {
            ctx.insert("review_pass".to_string(), serde_json::json!(3));
            ctx.insert("plan_content".to_string(), serde_json::json!("# Plan content"));
        }
        record.iteration = 5;

        let config = RuleOfFiveConfig::default();
        let executor = RuleOfFiveExecutor::from_record(&record, config).unwrap();

        assert_eq!(executor.current_pass(), Some(ReviewPass::EdgeCases));
        assert_eq!(executor.plan_content(), "# Plan content");
        assert_eq!(executor.total_iterations(), 5);
    }

    #[test]
    fn test_executor_config_default() {
        let config = RuleOfFiveConfig::default();
        assert_eq!(config.max_tokens, 16384);
        assert_eq!(config.max_iterations_per_pass, 3);
    }

    #[test]
    fn test_pass_result_structure() {
        let result = PassResult {
            pass: ReviewPass::Completeness,
            passed: true,
            response: "PASS".to_string(),
            updated_plan: None,
            issues: vec![],
            iterations: 1,
        };

        assert_eq!(result.pass, ReviewPass::Completeness);
        assert!(result.passed);
    }

    #[test]
    fn test_extract_text_from_response() {
        let response = CompletionResponse {
            content: Some("First part\nSecond part".to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        };

        let text = extract_text_from_response(&response);
        assert!(text.contains("First part"));
        assert!(text.contains("Second part"));
    }

    #[test]
    fn test_executor_error_display() {
        assert_eq!(
            format!("{}", ExecutorError::AlreadyComplete),
            "All passes already complete"
        );
        assert_eq!(
            format!("{}", ExecutorError::LlmError("test".to_string())),
            "LLM error: test"
        );
        assert_eq!(
            format!("{}", ExecutorError::InvalidState("bad".to_string())),
            "Invalid state: bad"
        );
    }

    #[test]
    fn test_executor_total_iterations() {
        let plan = make_test_plan();
        let config = RuleOfFiveConfig::default();
        let mut executor = RuleOfFiveExecutor::new(plan, config);

        assert_eq!(executor.total_iterations(), 0);

        executor.process_response("PASS: Good.", None);
        assert_eq!(executor.total_iterations(), 1);

        executor.process_response("NEEDS_WORK: Issue.", None);
        assert_eq!(executor.total_iterations(), 2);

        executor.process_response("PASS: Fixed.", None);
        assert_eq!(executor.total_iterations(), 3);
    }
}
